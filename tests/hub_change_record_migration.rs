//! Feature-specific migration, rollback and cold-restore qualification for change records.
#[path = "common/hub_pg.rs"]
mod hub_pg;

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use agentscm::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, AgentHarnessV1, AssuranceBasisV1,
    CaptureBatchV1, CaptureModeV1, CapturedEventKindV1, CapturedEventV1, CheckResultV1, CoverageV1,
    DriftV1, RecipeProvenanceV1, SessionIdentityV1, SourceBindingV1,
};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_agent_token, unix_now};
use agentscm::hub::work::{WorkRole, WorkStart};
use hub_pg::PgSchema;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const REPOSITORY: &str = "synthetic/change-record-migration";

struct Db {
    url: String,
    pg_schema: Option<String>,
    _pg: Option<PgSchema>,
    sqlite_path: Option<PathBuf>,
    _dir: Option<TempDir>,
}

impl Db {
    fn sqlite() -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hub.sqlite");
        Self {
            url: format!("sqlite://{}", path.display()),
            pg_schema: None,
            _pg: None,
            sqlite_path: Some(path),
            _dir: Some(dir),
        }
    }

    fn postgres() -> Option<Self> {
        let pg = PgSchema::create()?;
        Some(Self {
            url: pg.url.clone(),
            pg_schema: Some(pg.schema.clone()),
            _pg: Some(pg),
            sqlite_path: None,
            _dir: None,
        })
    }

    fn open(&self) -> Arc<dyn Store> {
        store::open(&self.url).unwrap()
    }

    fn execute(&self, sql: &str) -> Result<(), String> {
        if self.pg_schema.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .batch_execute(sql)
                .map_err(|error| {
                    error
                        .as_db_error()
                        .map_or_else(|| error.to_string(), |db| db.message().to_owned())
                })
        } else {
            rusqlite::Connection::open(self.sqlite_path.as_ref().unwrap())
                .unwrap()
                .execute_batch(sql)
                .map_err(|error| error.to_string())
        }
    }

    fn scalar(&self, sql: &str) -> i64 {
        if self.pg_schema.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .query_one(sql, &[])
                .unwrap()
                .get(0)
        } else {
            rusqlite::Connection::open(self.sqlite_path.as_ref().unwrap())
                .unwrap()
                .query_row(sql, [], |row| row.get(0))
                .unwrap()
        }
    }

    fn table_exists(&self, name: &str) -> bool {
        let sql = if self.pg_schema.is_some() {
            format!(
                "SELECT count(*) FROM information_schema.tables WHERE table_schema = current_schema() AND table_name = '{name}'"
            )
        } else {
            format!("SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = '{name}'")
        };
        self.scalar(&sql) == 1
    }

    fn index_exists(&self, name: &str) -> bool {
        let sql = if self.pg_schema.is_some() {
            format!(
                "SELECT count(*) FROM pg_indexes WHERE schemaname = current_schema() AND indexname = '{name}'"
            )
        } else {
            format!("SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = '{name}'")
        };
        self.scalar(&sql) == 1
    }

    fn identities(&self) -> Vec<String> {
        let sql = "SELECT 'operation:' || operation_id FROM hub_change_operations
                   UNION ALL SELECT 'session:' || session_id || ':' || CAST(generation AS TEXT) FROM hub_change_sessions
                   UNION ALL SELECT 'event:' || event_id FROM hub_change_events
                   UNION ALL SELECT 'action:' || action_id FROM hub_change_actions
                   ORDER BY 1";
        if self.pg_schema.is_some() {
            postgres::Client::connect(&self.url, postgres::NoTls)
                .unwrap()
                .query(sql, &[])
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect()
        } else {
            let connection =
                rusqlite::Connection::open(self.sqlite_path.as_ref().unwrap()).unwrap();
            let mut statement = connection.prepare(sql).unwrap();
            statement
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect()
        }
    }
}

struct Fixture {
    token: String,
    work_id: String,
    generation: i64,
}

fn seed_representative_work(db: &Db) -> Fixture {
    let store = db.open();
    let user = store
        .upsert_user(
            &ForgeIdentity {
                provider: "github".into(),
                host: "github.com".into(),
                user_id: 7701,
            },
            "migration-fixture",
        )
        .unwrap();
    let (token, _) =
        mint_agent_token(&*store, &user.id, "migration-writer", REPOSITORY, 3600).unwrap();
    let item = store
        .work_start(
            &token,
            &WorkStart {
                work_id: None,
                source: None,
                request_key: Some("representative-pre-change-work".into()),
                title: "retain a representative pre-change Work".into(),
                session: "migration-session".into(),
                role: WorkRole::Lead,
                scope: vec!["src/**".into()],
                branch: Some("feature/migration-fixture".into()),
                worktree: Some("migration-worktree".into()),
                next_action: Some("write and recover one change record".into()),
                depends_on: vec![],
                related_work: vec![],
                takeover_generation: None,
                preflight: None,
            },
            unix_now(),
        )
        .unwrap()
        .item;
    Fixture {
        token,
        work_id: item.id,
        generation: item.generation,
    }
}

fn remove_change_records(db: &Db) {
    db.execute(
        "DROP TABLE hub_change_shared_bodies;
         DROP TABLE hub_change_events;
         DROP TABLE hub_change_actions;
         DROP TABLE hub_change_sessions;
         DROP TABLE hub_change_operations;
         DELETE FROM hub_named_migrations WHERE name IN ('change_records', 'shared_conversations');",
    )
    .unwrap();
    assert_eq!(
        db.scalar("SELECT count(*) FROM hub_named_migrations WHERE name = 'change_records'"),
        0
    );
}

fn representative_pre_change(db: &Db) -> Fixture {
    let fixture = seed_representative_work(db);
    // Removing only the new named migration from a quiescent current fixture yields the exact
    // schema/data boundary a pre-change binary leaves: all integer migrations, prior named
    // migrations, and a live Work remain, while no change-record table or marker exists.
    remove_change_records(db);
    assert_eq!(db.scalar("SELECT count(*) FROM hub_work_items"), 1);
    fixture
}

fn write_change_record(store: &dyn Store, fixture: &Fixture) -> Value {
    let now = unix_now();
    let session = SessionIdentityV1 {
        harness: AgentHarnessV1::Codex,
        adapter_version: "migration-fixture-v1".into(),
        session_id: "migration-session".into(),
        parent_session_id: None,
        capture_mode: CaptureModeV1::MetadataOnly,
    };
    store
        .change_ingest(
            &fixture.token,
            &CaptureBatchV1 {
                schema_version: 1,
                operation_id: "migration:event:start".into(),
                work_id: fixture.work_id.clone(),
                generation: fixture.generation,
                session,
                events: vec![CapturedEventV1 {
                    event_id: "migration-session-start".into(),
                    sequence: 1,
                    observed_at: now,
                    kind: CapturedEventKindV1::SessionStarted,
                    tool: None,
                    artifact: None,
                    setup: None,
                    gap: None,
                    boundary: None,
                    subagent: None,
                }],
            },
            now,
        )
        .unwrap();
    store
        .change_action_start(
            &fixture.token,
            &ActionStartV1 {
                schema_version: 1,
                operation_id: "migration:action:start".into(),
                work_id: fixture.work_id.clone(),
                generation: fixture.generation,
                session_id: "migration-session".into(),
                action_id: "migration-action".into(),
                source: SourceBindingV1::Git {
                    repository: REPOSITORY.into(),
                    worktree_id: "migration-worktree".into(),
                    head: "1".repeat(40),
                    tree: "2".repeat(40),
                    base: "3".repeat(40),
                    dirty: false,
                    untracked_count: 0,
                    omissions: vec![],
                    captured_snapshot: None,
                    snapshot_coverage: CoverageV1::Complete,
                },
                kind: ActionKindV1::Verification,
                recipe_provenance: RecipeProvenanceV1::ConfiguredExactMatch,
                recipe_digest: "4".repeat(64),
                setup_digest: "5".repeat(64),
                observed_order: 1,
                observation_scope_id: Some("migration-observer".into()),
                started_at: now,
            },
            now + 1,
        )
        .unwrap();
    store
        .change_action_finish(
            &fixture.token,
            &ActionFinishV1 {
                schema_version: 1,
                operation_id: "migration:action:finish".into(),
                work_id: fixture.work_id.clone(),
                generation: fixture.generation,
                action_id: "migration-action".into(),
                outcome: ActionOutcomeV1::Succeeded,
                result: CheckResultV1::Passed,
                assurance: AssuranceBasisV1::CallerAsserted,
                coverage: CoverageV1::Complete,
                finished_at: now + 2,
                drift: DriftV1::NoneObserved,
                output_artifacts: vec![],
            },
            now + 2,
        )
        .unwrap();
    serde_json::to_value(
        store
            .change_record(&fixture.token, &fixture.work_id, now + 3)
            .unwrap()
            .unwrap(),
    )
    .unwrap()
}

fn assert_migrated_schema(db: &Db) {
    assert_eq!(
        db.scalar("SELECT count(*) FROM hub_named_migrations WHERE name = 'change_records'"),
        1
    );
    for table in [
        "hub_change_operations",
        "hub_change_sessions",
        "hub_change_events",
        "hub_change_actions",
        "hub_change_shared_bodies",
    ] {
        assert!(db.table_exists(table), "missing {table}");
    }
    assert_eq!(
        db.scalar("SELECT count(*) FROM hub_named_migrations WHERE name = 'shared_conversations'"),
        1
    );
    assert!(db.index_exists("hub_change_shared_bodies_expiry"));
    assert!(db.index_exists("hub_change_shared_bodies_sweep"));
    assert!(db.index_exists("hub_change_events_page"));
    assert!(db.index_exists("hub_change_actions_latest"));
}

fn assert_same_projection(mut actual: Value, mut expected: Value) {
    assert!(actual["as_of"].is_number());
    assert!(expected["as_of"].is_number());
    actual.as_object_mut().unwrap().remove("as_of");
    expected.as_object_mut().unwrap().remove("as_of");
    assert_eq!(actual, expected);
}

fn assert_old_binary_refuses(database_url: &str) -> bool {
    let Some(binary) = std::env::var_os("AGENTSCM_TEST_OLD_BINARY") else {
        eprintln!("skipping exact older-binary refusal: baseline binary unset");
        return false;
    };
    let run = TempDir::new().unwrap();
    for directory in ["data", "objects", "secrets"] {
        std::fs::create_dir(run.path().join(directory)).unwrap();
    }
    let stdout_path = run.path().join("stdout");
    let stderr_path = run.path().join("stderr");
    let stdout = std::fs::File::create(&stdout_path).unwrap();
    let stderr = std::fs::File::create(&stderr_path).unwrap();
    let mut child = Command::new(binary)
        .env_clear()
        .env("AGENTSCM_DATABASE_URL", database_url)
        .arg("hub")
        .arg("serve")
        .arg("--no-telemetry")
        .args(["--bind", "127.0.0.1:0"])
        .arg("--data-dir")
        .arg(run.path().join("data"))
        .arg("--objects")
        .arg(run.path().join("objects"))
        .arg("--secrets")
        .arg(run.path().join("secrets"))
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("older binary did not refuse the newer database within five seconds");
        }
        thread::sleep(Duration::from_millis(25));
    };
    assert!(
        !status.success(),
        "older binary unexpectedly accepted schema"
    );
    let stdout = std::fs::read(stdout_path).unwrap();
    let stderr = std::fs::read(stderr_path).unwrap();
    let mut output = stdout;
    output.push(b'\n');
    output.extend_from_slice(&stderr);
    if !String::from_utf8_lossy(&output).contains("database schema is newer than this binary") {
        panic!(
            "unexpected older-binary refusal; output bytes {}; sha256 {}",
            output.len(),
            Sha256::digest(&output)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
    }
    eprintln!("older binary refused newer named schema as expected");
    true
}

fn file_digest(path: &std::path::Path) -> Vec<u8> {
    Sha256::digest(std::fs::read(path).unwrap()).to_vec()
}

fn representative_upgrade_roundtrip(db: &Db) {
    let fixture = representative_pre_change(db);
    let store = db.open();
    assert_migrated_schema(db);
    assert!(
        store
            .work_get(&fixture.token, &fixture.work_id, unix_now())
            .unwrap()
            .is_some()
    );
    let expected = write_change_record(&*store, &fixture);
    let identities = db.identities();
    drop(store);

    let reopened = db.open();
    assert_migrated_schema(db);
    assert_same_projection(
        serde_json::to_value(
            reopened
                .change_record(&fixture.token, &fixture.work_id, unix_now())
                .unwrap()
                .unwrap(),
        )
        .unwrap(),
        expected,
    );
    assert_eq!(db.identities(), identities);
}

fn interrupted_migration_rolls_back_and_retries(db: &Db) {
    let fixture = representative_pre_change(db);
    db.execute("CREATE TABLE hub_change_events (collision INTEGER)")
        .unwrap();
    store::open(&db.url)
        .err()
        .expect("the incompatible table must fail migration");
    assert_eq!(
        db.scalar("SELECT count(*) FROM hub_named_migrations WHERE name = 'change_records'"),
        0
    );
    assert!(!db.table_exists("hub_change_operations"));
    assert!(!db.table_exists("hub_change_sessions"));
    assert!(!db.table_exists("hub_change_actions"));
    assert!(db.table_exists("hub_change_events"));

    db.execute("DROP TABLE hub_change_events").unwrap();
    let store = db.open();
    assert_migrated_schema(db);
    let projection = write_change_record(&*store, &fixture);
    assert_eq!(projection["latest_action"]["result"], "passed");
}

#[test]
fn sqlite_representative_upgrade_and_change_record_roundtrip() {
    representative_upgrade_roundtrip(&Db::sqlite());
}

#[test]
fn sqlite_interrupted_change_record_migration_rolls_back_and_retries() {
    interrupted_migration_rolls_back_and_retries(&Db::sqlite());
}

#[test]
fn sqlite_cold_restore_preserves_change_record_rows_and_projection() {
    let db = Db::sqlite();
    let fixture = representative_pre_change(&db);
    let store = db.open();
    let expected = write_change_record(&*store, &fixture);
    let identities = db.identities();
    drop(store);

    let source = db.sqlite_path.as_ref().unwrap();
    rusqlite::Connection::open(source)
        .unwrap()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    let restore_dir = TempDir::new().unwrap();
    let restored_path = restore_dir.path().join("restored.sqlite");
    std::fs::copy(source, &restored_path).unwrap();
    let restored = Db {
        url: format!("sqlite://{}", restored_path.display()),
        pg_schema: None,
        _pg: None,
        sqlite_path: Some(restored_path),
        _dir: Some(restore_dir),
    };
    let restored_store = restored.open();
    assert_same_projection(
        serde_json::to_value(
            restored_store
                .change_record(&fixture.token, &fixture.work_id, unix_now())
                .unwrap()
                .unwrap(),
        )
        .unwrap(),
        expected,
    );
    assert_eq!(restored.identities(), identities);
    drop(restored_store);
    let before = file_digest(restored.sqlite_path.as_ref().unwrap());
    if assert_old_binary_refuses(&restored.url) {
        assert_eq!(file_digest(restored.sqlite_path.as_ref().unwrap()), before);
    }
}

mod on_postgres {
    use super::*;
    use postgres::config::{Host, SslMode};
    use std::fs::{File, OpenOptions};
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    use std::path::Path;
    use std::str::FromStr;
    use std::sync::Mutex;

    const CLIENT_SERVICE: &str = "agentscm_change_record_qualification";
    const FINGERPRINT_RESTRICT_KEY: &str =
        "cb839bafe2db1994161d1bdd273c584c8de216356ad42ff74839a5b759579d9e";
    // pg_dump reads shared catalogs even when selecting one schema. Prevent another destructive
    // fixture in this binary from dropping its schema during that catalog snapshot.
    static POSTGRES_TEST_LOCK: Mutex<()> = Mutex::new(());

    struct PrivateCredentialDirectory {
        path: Option<PathBuf>,
        parent: File,
    }

    impl PrivateCredentialDirectory {
        fn create(parent: &Path, suffix: &str) -> Self {
            let parent_handle = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(parent)
                .unwrap();
            let directory = (0..16)
                .find_map(|_| {
                    let mut random = [0_u8; 16];
                    getrandom::fill(&mut random).unwrap();
                    let name = format!(
                        "libpq-{suffix}-{}",
                        random
                            .iter()
                            .map(|byte| format!("{byte:02x}"))
                            .collect::<String>()
                    );
                    let path = parent.join(name);
                    let mut builder = std::fs::DirBuilder::new();
                    builder.mode(0o700);
                    match builder.create(&path) {
                        Ok(()) => Some(path),
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                        Err(error) => {
                            panic!("private credential directory creation failed: {error}")
                        }
                    }
                })
                .expect("private credential directory names repeatedly collided");
            let credentials = Self {
                path: Some(directory),
                parent: parent_handle,
            };
            assert_eq!(
                std::fs::metadata(credentials.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            credentials.parent.sync_all().unwrap();
            credentials
        }

        fn path(&self) -> &Path {
            self.path.as_deref().unwrap()
        }

        fn sync(&self) {
            OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(self.path())
                .unwrap()
                .sync_all()
                .unwrap();
        }
    }

    impl Drop for PrivateCredentialDirectory {
        fn drop(&mut self) {
            if let Some(directory) = self.path.take() {
                let removed = std::fs::remove_dir_all(directory);
                let synced = self.parent.sync_all();
                if !std::thread::panicking() {
                    removed.unwrap();
                    synced.unwrap();
                }
            }
        }
    }

    #[derive(Clone, Copy)]
    enum CredentialConstructionFault {
        None,
        ReturnAfterService,
        PanicAfterService,
    }

    struct LibpqClientContext {
        credentials: PrivateCredentialDirectory,
        service_file: PathBuf,
        pass_file: PathBuf,
    }

    impl LibpqClientContext {
        fn create(directory: &Path, database_url: &str, suffix: &str) -> Self {
            Self::create_inner(
                directory,
                database_url,
                suffix,
                CredentialConstructionFault::None,
            )
            .unwrap()
        }

        fn create_inner(
            directory: &Path,
            database_url: &str,
            suffix: &str,
            fault: CredentialConstructionFault,
        ) -> Result<Self, &'static str> {
            assert!(
                !suffix.is_empty()
                    && suffix.len() <= 96
                    && suffix
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
                "qualification libpq filename suffix must be a bounded basename"
            );
            let config = postgres::Config::from_str(database_url)
                .expect("PostgreSQL qualification URL must be parseable");
            assert_eq!(
                config.get_hosts().len(),
                1,
                "qualification requires exactly one PostgreSQL host"
            );
            assert!(
                config.get_ports().len() <= 1,
                "qualification requires exactly one PostgreSQL port"
            );
            let (host, pass_host) = match &config.get_hosts()[0] {
                Host::Tcp(host) => {
                    reject_libpq_control(host.as_bytes(), "PostgreSQL host");
                    (host.clone(), host.clone())
                }
                Host::Unix(path) => {
                    let host = path
                        .to_str()
                        .expect("qualification Unix socket path must be UTF-8")
                        .to_owned();
                    reject_libpq_control(host.as_bytes(), "PostgreSQL Unix socket path");
                    (host, "localhost".to_owned())
                }
            };
            let port = config.get_ports().first().copied().unwrap_or(5432);
            let user = config
                .get_user()
                .expect("qualification PostgreSQL URL must name a user");
            let database = config
                .get_dbname()
                .expect("qualification PostgreSQL URL must name a database");
            reject_libpq_control(user.as_bytes(), "PostgreSQL user");
            reject_libpq_control(database.as_bytes(), "PostgreSQL database");
            let password = config
                .get_password()
                .map(|password| {
                    std::str::from_utf8(password)
                        .expect("qualification PostgreSQL password must be UTF-8")
                })
                .unwrap_or("");
            reject_libpq_control(password.as_bytes(), "PostgreSQL password");
            let sslmode = match config.get_ssl_mode() {
                SslMode::Disable => "disable",
                SslMode::Prefer => "prefer",
                SslMode::Require => "require",
                _ => panic!("unsupported PostgreSQL sslmode for qualification clients"),
            };

            let credentials = PrivateCredentialDirectory::create(directory, suffix);
            let service_file = credentials.path().join("service.conf");
            let pass_file = credentials.path().join("pass");
            write_private(
                &service_file,
                format!(
                    "[{CLIENT_SERVICE}]\nhost={}\nport={port}\nuser={}\ndbname={}\nsslmode={sslmode}\nconnect_timeout=5\n",
                    service_value(&host),
                    service_value(user),
                    service_value(database),
                )
                .as_bytes(),
            );
            credentials.sync();
            match fault {
                CredentialConstructionFault::ReturnAfterService => {
                    return Err("injected credential construction failure");
                }
                CredentialConstructionFault::PanicAfterService => {
                    panic!("injected credential construction panic");
                }
                CredentialConstructionFault::None => {}
            }
            write_private(
                &pass_file,
                format!(
                    "{}:{port}:{}:{}:{}\n",
                    pgpass_value(&pass_host),
                    pgpass_value(database),
                    pgpass_value(user),
                    pgpass_value(password),
                )
                .as_bytes(),
            );
            credentials.sync();
            Ok(Self {
                credentials,
                service_file,
                pass_file,
            })
        }

        fn command(&self, executable: &Path) -> Command {
            let mut command = Command::new(executable);
            command
                .env_clear()
                .env("LC_ALL", "C")
                .env("PGSERVICE", CLIENT_SERVICE)
                .env("PGSERVICEFILE", &self.service_file)
                .env("PGPASSFILE", &self.pass_file);
            command
        }

        fn credential_directory(&self) -> &Path {
            self.credentials.path()
        }
    }

    fn reject_libpq_control(value: &[u8], label: &str) {
        assert!(
            value.len() <= 32 * 1024
                && !value
                    .iter()
                    .any(|byte| matches!(byte, b'\0' | b'\n' | b'\r')),
            "{label} is oversized or contains unsupported control bytes"
        );
    }

    fn service_value(value: &str) -> String {
        assert!(
            !value.is_empty()
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric()
                        || matches!(byte, b'.' | b'_' | b'-' | b'/' | b':' | b'%')
                }),
            "qualification libpq service value contains unsupported syntax"
        );
        value.to_owned()
    }

    fn pgpass_value(value: &str) -> String {
        value.replace('\\', "\\\\").replace(':', "\\:")
    }

    fn shell_single_quote(value: &Path) -> String {
        format!(
            "'{}'",
            value
                .to_str()
                .expect("test path must be UTF-8")
                .replace('\'', "'\\''")
        )
    }

    fn write_private(path: &Path, bytes: &[u8]) {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    fn pg_tool(variable: &str, name: &str) -> PathBuf {
        if let Some(path) = std::env::var_os(variable) {
            let path = PathBuf::from(path);
            assert!(path.is_absolute(), "{variable} must be an absolute path");
            return path;
        }
        for parent in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin"] {
            let path = Path::new(parent).join(name);
            if path.is_file() {
                return path;
            }
        }
        panic!("{variable} must name the absolute {name} executable");
    }

    fn run_client(mut command: Command, label: &str) -> Result<std::process::Output, String> {
        let output = command
            .output()
            .map_err(|error| format!("{label} could not start: {error}"))?;
        if output.status.success() {
            return Ok(output);
        }
        Err(format!(
            "{label} failed with status {:?}; stderr bytes {}; stderr sha256 {}",
            output.status.code(),
            output.stderr.len(),
            Sha256::digest(&output.stderr)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    }

    fn database_fingerprint(dump: &Path, auth: &LibpqClientContext) -> Vec<u8> {
        let mut command = auth.command(dump);
        command.args([
            "--format=plain",
            "--no-owner",
            "--no-privileges",
            "--no-comments",
            "--quote-all-identifiers",
            "--column-inserts",
        ]);
        command.arg(format!("--restrict-key={FINGERPRINT_RESTRICT_KEY}"));
        let output = run_client(command, "pg_dump logical fingerprint").unwrap();
        Sha256::digest(output.stdout).to_vec()
    }

    struct CloneDatabase {
        base_url: String,
        name: String,
        url: String,
        cleaned: bool,
    }

    fn connect_bounded(database_url: &str) -> postgres::Client {
        let mut config = postgres::Config::from_str(database_url).unwrap();
        config.connect_timeout(Duration::from_secs(5));
        config.connect(postgres::NoTls).unwrap()
    }

    impl CloneDatabase {
        fn create(base_url: &str, source_url: &str) -> Self {
            let mut random = [0_u8; 8];
            getrandom::fill(&mut random).unwrap();
            let name = format!(
                "agentscm_cr_restore_{}",
                random
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            connect_bounded(base_url)
                .batch_execute(&format!("CREATE DATABASE {name}"))
                .expect("qualification user must be able to create a disposable database");
            let mut parsed = reqwest::Url::parse(source_url).unwrap();
            parsed.set_path(&format!("/{name}"));
            Self {
                base_url: base_url.to_owned(),
                name,
                url: parsed.to_string(),
                cleaned: false,
            }
        }

        fn cleanup(&mut self) {
            connect_bounded(&self.base_url)
                .batch_execute(&format!("DROP DATABASE {}", self.name))
                .expect("disposable qualification database must drop cleanly");
            self.cleaned = true;
        }
    }

    impl Drop for CloneDatabase {
        fn drop(&mut self) {
            if !self.cleaned
                && let Ok(mut config) = postgres::Config::from_str(&self.base_url)
            {
                config.connect_timeout(Duration::from_secs(5));
                if let Ok(mut client) = config.connect(postgres::NoTls) {
                    let _ = client.batch_execute(&format!("DROP DATABASE {}", self.name));
                }
            }
        }
    }

    fn isolated_schema(database_url: &str) -> Db {
        let mut random = [0_u8; 8];
        getrandom::fill(&mut random).unwrap();
        let schema = format!(
            "hub_t_{}",
            random
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
        connect_bounded(database_url)
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .unwrap();
        let separator = if database_url.contains('?') { '&' } else { '?' };
        Db {
            url: format!(
                "{database_url}{separator}options=-c%20search_path%3D{schema}&application_name={schema}"
            ),
            pg_schema: Some(schema),
            _pg: None,
            sqlite_path: None,
            _dir: None,
        }
    }

    #[test]
    fn representative_upgrade_and_change_record_roundtrip() {
        let _guard = POSTGRES_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(db) = Db::postgres() {
            super::representative_upgrade_roundtrip(&db);
        }
    }

    #[test]
    fn interrupted_change_record_migration_rolls_back_and_retries() {
        let _guard = POSTGRES_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(db) = Db::postgres() {
            super::interrupted_migration_rolls_back_and_retries(&db);
        }
    }

    #[test]
    fn postgres_clients_keep_passwords_out_of_argv_environment_and_errors() {
        let directory = TempDir::new().unwrap();
        let secret = "synthetic-password-not-for-output";
        assert!(
            std::panic::catch_unwind(|| {
                LibpqClientContext::create(
                    directory.path(),
                    "postgresql://fixture:bad%0Avalue@127.0.0.1:55432/fixture",
                    "injected-control",
                )
            })
            .is_err(),
            "decoded libpq control bytes must be rejected before files or a child process"
        );
        assert!(
            std::panic::catch_unwind(|| {
                LibpqClientContext::create(
                    directory.path(),
                    "postgresql://fixture%23comment@127.0.0.1:55432/fixture",
                    "injected-service-syntax",
                )
            })
            .is_err(),
            "libpq service syntax must be rejected rather than reinterpreted"
        );
        assert!(
            std::panic::catch_unwind(|| {
                LibpqClientContext::create(
                    directory.path(),
                    "postgresql://fixture@127.0.0.1:55432/fixture",
                    "../outside",
                )
            })
            .is_err(),
            "private credential filenames must remain single bounded basenames"
        );
        let failure_url = format!("postgresql://fixture:{secret}@127.0.0.1:55432/fixture");
        let injected_error = match LibpqClientContext::create_inner(
            directory.path(),
            &failure_url,
            "injected-error",
            CredentialConstructionFault::ReturnAfterService,
        ) {
            Ok(_) => panic!("injected credential construction failure unexpectedly succeeded"),
            Err(error) => error,
        };
        assert_eq!(injected_error, "injected credential construction failure");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        assert!(
            std::panic::catch_unwind(|| {
                let _ = LibpqClientContext::create_inner(
                    directory.path(),
                    &failure_url,
                    "injected-panic",
                    CredentialConstructionFault::PanicAfterService,
                );
            })
            .is_err()
        );
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let auth = LibpqClientContext::create(directory.path(), &failure_url, "privacy-shim");
        let credential_directory = auth.credential_directory().to_owned();
        assert!(credential_directory.is_dir());
        let capture = directory.path().join("shim-capture");
        let shim = directory.path().join("pg-client-shim");
        write_private(
            &shim,
            format!(
                "#!/bin/sh\n{{ /usr/bin/printf 'ARG=%s\\n' \"$@\"; /usr/bin/env; }} > {}\n/usr/bin/printf '%s\\n' '{secret}' >&2\nexit 7\n",
                shell_single_quote(&capture)
            )
            .as_bytes(),
        );
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();

        let mut command = auth.command(&shim);
        command.args(["--format=custom", "--schema", "synthetic_schema"]);
        let error = run_client(command, "synthetic pg client").unwrap_err();
        let observed = std::fs::read_to_string(capture).unwrap();
        assert!(!observed.contains(secret));
        assert!(!error.contains(secret));
        assert!(!observed.contains("postgresql://"));
        for forbidden in ["AWS_", "TOKEN=", "SECRET=", "DATABASE_URL="] {
            assert!(
                !observed.contains(forbidden),
                "sanitized child inherited forbidden environment name {forbidden}"
            );
        }
        assert!(
            !std::fs::read_to_string(&auth.service_file)
                .unwrap()
                .contains(secret)
        );
        assert!(
            std::fs::read_to_string(&auth.pass_file)
                .unwrap()
                .contains(secret)
        );
        assert_eq!(
            std::fs::metadata(&auth.service_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&auth.pass_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(auth);
        assert!(!credential_directory.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn backup_restore_preserves_change_record_rows_and_projection() {
        let _guard = POSTGRES_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(dump_directory) = std::env::var_os("AGENTSCM_TEST_PG_BACKUP_RESTORE_DIR") else {
            eprintln!("skipping PostgreSQL backup/restore: private dump directory unset");
            return;
        };
        let Some(configured_url) = std::env::var("AGENTSCM_TEST_DATABASE_URL")
            .ok()
            .filter(|url| !url.is_empty())
        else {
            eprintln!("skipping PostgreSQL backup/restore: test database URL unset");
            return;
        };
        let mut configured_base = reqwest::Url::parse(&configured_url).unwrap();
        configured_base.set_query(None);
        let configured_base = configured_base.to_string();
        // pg_dump reads shared catalogs even with --schema. Keep both destructive restore
        // fixtures in their own task-owned databases so other nextest processes cannot race the
        // snapshot by dropping an unrelated schema in the configured base database.
        let mut original_database = CloneDatabase::create(&configured_base, &configured_base);
        let db = isolated_schema(&original_database.url);
        let fixture = representative_pre_change(&db);
        let store = db.open();
        let expected = write_change_record(&*store, &fixture);
        let identities = db.identities();
        drop(store);

        let schema = db.pg_schema.as_ref().unwrap().clone();
        let dump_directory = PathBuf::from(dump_directory);
        std::fs::create_dir_all(&dump_directory).unwrap();
        std::fs::set_permissions(&dump_directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut base_url = reqwest::Url::parse(&db.url).unwrap();
        base_url.set_query(None);
        let base_url = base_url.to_string();
        let original_auth =
            LibpqClientContext::create(&dump_directory, &base_url, &format!("original-{schema}"));
        let original_credential_directory = original_auth.credential_directory().to_owned();
        let dump_tool = pg_tool("PG_DUMP", "pg_dump");
        let restore_tool = pg_tool("PG_RESTORE", "pg_restore");
        let dump_path = dump_directory.join(format!("{schema}.dump"));
        OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&dump_path)
            .unwrap();
        let mut dump = original_auth.command(&dump_tool);
        dump.args([
            "--format=custom",
            "--no-owner",
            "--no-privileges",
            "--schema",
        ])
        .arg(&schema)
        .arg("--file")
        .arg(&dump_path);
        run_client(dump, "pg_dump custom backup").unwrap();

        connect_bounded(&original_database.url)
            .batch_execute(&format!("DROP SCHEMA {schema} CASCADE"))
            .unwrap();
        let mut restore = original_auth.command(&restore_tool);
        restore
            .args(["--no-owner", "--no-privileges", "--dbname"])
            .arg(format!("service={CLIENT_SERVICE}"))
            .arg(&dump_path);
        run_client(restore, "pg_restore original qualification schema").unwrap();

        let restored = db.open();
        assert_same_projection(
            serde_json::to_value(
                restored
                    .change_record(&fixture.token, &fixture.work_id, unix_now())
                    .unwrap()
                    .unwrap(),
            )
            .unwrap(),
            expected,
        );
        assert_eq!(db.identities(), identities);
        drop(restored);

        // Preserve the restored qualification schema above. The older-binary refusal runs only
        // against a second, uniquely named disposable database restored from the same dump.
        let mut clone = CloneDatabase::create(&base_url, &db.url);
        let clone_auth = LibpqClientContext::create(
            &dump_directory,
            &clone.url,
            &format!("refusal-{}", clone.name),
        );
        let clone_credential_directory = clone_auth.credential_directory().to_owned();
        let mut clone_restore = clone_auth.command(&restore_tool);
        clone_restore
            .args(["--no-owner", "--no-privileges", "--dbname"])
            .arg(format!("service={CLIENT_SERVICE}"))
            .arg(&dump_path);
        run_client(clone_restore, "pg_restore older-binary refusal clone").unwrap();

        let before = database_fingerprint(&dump_tool, &clone_auth);
        assert_eq!(
            database_fingerprint(&dump_tool, &clone_auth),
            before,
            "logical fingerprint must be deterministic before refusal"
        );
        if assert_old_binary_refuses(&clone.url) {
            let after = database_fingerprint(&dump_tool, &clone_auth);
            assert_eq!(
                after, before,
                "older binary changed the disposable restored database"
            );
        }

        let mut client = postgres::Client::connect(&clone.url, postgres::NoTls).unwrap();
        assert!(
            client
                .execute(
                    "UPDATE hub_users SET login = login || '-fingerprint-self-test'",
                    &[],
                )
                .unwrap()
                > 0
        );
        drop(client);
        assert_ne!(
            database_fingerprint(&dump_tool, &clone_auth),
            before,
            "logical fingerprint failed to detect an unrelated retained-row mutation"
        );
        client = postgres::Client::connect(&clone.url, postgres::NoTls).unwrap();
        client
            .batch_execute(
                "CREATE TABLE public.agentscm_fingerprint_probe (id BIGINT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO public.agentscm_fingerprint_probe (id, value) VALUES (1, 'before');",
            )
            .unwrap();
        drop(client);
        let outside_schema = database_fingerprint(&dump_tool, &clone_auth);
        let mut client = postgres::Client::connect(&clone.url, postgres::NoTls).unwrap();
        client
            .execute(
                "UPDATE public.agentscm_fingerprint_probe SET value = 'after' WHERE id = 1",
                &[],
            )
            .unwrap();
        drop(client);
        assert_ne!(
            database_fingerprint(&dump_tool, &clone_auth),
            outside_schema,
            "logical fingerprint failed to detect data mutation outside the restored schema"
        );
        drop(clone_auth);
        assert!(!clone_credential_directory.exists());
        drop(original_auth);
        assert!(!original_credential_directory.exists());
        clone.cleanup();
        drop(db);
        original_database.cleanup();
    }
}

fn shared_body_digest_must_be_lower_hex(db: &Db) {
    drop(db.open());
    let insert = |digest: &str| {
        db.execute(&format!(
            "INSERT INTO hub_change_shared_bodies
             (work, digest, author, size_bytes, media, session_id, generation, event_id, sequence,
              declared_token, declared_at, state)
             VALUES ('work_none', '{digest}', 'usr_none', 1, 'json', 's', 1, 'e', 1, 'tok_none', 0,
                     'declared')"
        ))
        .unwrap_err()
        .to_lowercase()
    };
    // A well-formed digest gets past the checks and meets the foreign key instead.
    assert!(insert(&"a".repeat(64)).contains("foreign key"));
    for bad in [
        "A".repeat(64),
        "g".repeat(64),
        "a".repeat(63),
        "a".repeat(65),
    ] {
        assert!(insert(&bad).contains("check"), "{bad}");
    }
}

#[test]
fn sqlite_shared_body_digest_must_be_lower_hex() {
    shared_body_digest_must_be_lower_hex(&Db::sqlite());
}

#[test]
fn postgres_shared_body_digest_must_be_lower_hex() {
    if let Some(db) = Db::postgres() {
        shared_body_digest_must_be_lower_hex(&db);
    }
}
