use std::path::PathBuf;
use std::process::Command;

use agentscm::team::service::DatabaseConfig;
use postgres::{Client, NoTls};
use tempfile::TempDir;

/// Every qualification owns a new cluster/socket and disables TCP. Never
/// connect to an existing listener, database or profile to satisfy this gate.
pub struct Cluster {
    pub directory: TempDir,
    pub config: DatabaseConfig,
    binaries: PathBuf,
}
impl Cluster {
    pub fn new() -> Self {
        let binaries = PathBuf::from(
            std::env::var_os("AGENTSCM_PG_BIN_DIR")
                .expect("set AGENTSCM_PG_BIN_DIR to an explicit PostgreSQL18 binary directory"),
        );
        assert!(
            binaries.is_absolute(),
            "PG binary directory must be absolute"
        );
        let version = Command::new(binaries.join("postgres"))
            .arg("--version")
            .output()
            .expect("PG18 binary required");
        assert!(version.status.success());
        assert!(
            String::from_utf8_lossy(&version.stdout).starts_with("postgres (PostgreSQL) 18."),
            "this qualification requires PostgreSQL18"
        );
        let directory = tempfile::Builder::new()
            .prefix("agentscm-auth-pg-")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = directory.path().join("socket");
        std::fs::create_dir(&socket).unwrap();
        let result = Command::new(binaries.join("initdb"))
            .arg("-D")
            .arg(directory.path().join("data"))
            .args([
                "--no-locale",
                "--encoding=UTF8",
                "--auth-local=trust",
                "--auth-host=reject",
            ])
            .output()
            .unwrap();
        assert!(result.status.success(), "owned initdb failed");
        let config = DatabaseConfig {
            socket,
            port: 5432,
            database: "postgres".into(),
        };
        let cluster = Self {
            directory,
            config,
            binaries,
        };
        cluster.start();
        cluster
    }
    pub fn admin(&self) -> Client {
        postgres::Config::new()
            .host_path(&self.config.socket)
            .port(self.config.port)
            .dbname(&self.config.database)
            .connect(NoTls)
            .unwrap()
    }
    fn start(&self) {
        let options = format!(
            "-k {} -p 5432 -c listen_addresses='' -c max_connections=16",
            self.config.socket.display()
        );
        let result = Command::new(self.binaries.join("pg_ctl"))
            .arg("-D")
            .arg(self.directory.path().join("data"))
            .arg("-l")
            .arg(self.directory.path().join("postgres.log"))
            .args(["-w", "-t", "15", "-o", &options, "start"])
            .output()
            .unwrap();
        assert!(result.status.success(), "owned cluster startup failed");
    }
}
impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = Command::new(self.binaries.join("pg_ctl"))
            .arg("-D")
            .arg(self.directory.path().join("data"))
            .args(["-w", "-t", "15", "-m", "fast", "stop"])
            .output();
    }
}
