//! Test-only seeder for the container end-to-end test (`.github/hub-e2e.sh`). Not shipped:
//! the product image never contains it (see the `e2e-seed` stage in the Dockerfile).
//!
//! It writes two users and two agent tokens straight into the hub database through the
//! library, so events are recorded as in production, and stores each token secret in
//! `$AGENTSCM_SEED_DIR/a.token` and `b.token` (mode 0600). A real hub needs a forge sign-in
//! to do this; the e2e test has no forge. With `AGENTSCM_SEED_S3_BUCKET` set it also creates
//! that bucket on the S3 endpoint named by the `AGENTSCM_S3_*` variables.
use std::error::Error;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::thread::sleep;
use std::time::Duration;

use agentscm::backend_s3::{S3Backend, S3Config};
use agentscm::hub::store::{self, ForgeIdentity, mint_agent_token};

const DEFAULT_DATABASE_URL: &str = "sqlite:///data/hub.sqlite";
const REPOSITORY: &str = "acme/widgets";

fn env(name: &str) -> Result<String, Box<dyn Error>> {
    std::env::var(name).map_err(|_| format!("{name} must be set").into())
}

fn write_secret(path: &Path, secret: &str) -> Result<(), Box<dyn Error>> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "{secret}")?;
    Ok(())
}

fn create_bucket(bucket: String) -> Result<(), Box<dyn Error>> {
    let backend = S3Backend::open(S3Config {
        endpoint: env("AGENTSCM_S3_ENDPOINT")?,
        region: "us-east-1".into(),
        bucket,
        prefix: String::new(),
        access_key_id: env("AGENTSCM_S3_ACCESS_KEY_ID")?,
        secret_access_key: env("AGENTSCM_S3_SECRET_ACCESS_KEY")?,
    })?;
    // The S3 server may still be starting.
    for _ in 0..9 {
        if backend.create_bucket().is_ok() {
            return Ok(());
        }
        sleep(Duration::from_secs(2));
    }
    Ok(backend.create_bucket()?)
}

fn main() -> Result<(), Box<dyn Error>> {
    let url =
        std::env::var("AGENTSCM_DATABASE_URL").unwrap_or_else(|_| DEFAULT_DATABASE_URL.into());
    let dir = env("AGENTSCM_SEED_DIR")?;
    let store = store::open(&url)?;
    for (file, user_id, login, agent) in [
        ("a", 1001, "alice", "claude-a"),
        ("b", 1002, "bob", "claude-b"),
    ] {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id,
        };
        let user = store.upsert_user(&identity, login)?;
        let (token_id, secret) = mint_agent_token(&*store, &user.id, agent, REPOSITORY, 3600)?;
        let path = Path::new(&dir).join(format!("{file}.token"));
        write_secret(&path, secret.expose())?;
        println!("{login}: token {token_id} written to {}", path.display());
    }
    if let Ok(bucket) = std::env::var("AGENTSCM_SEED_S3_BUCKET") {
        create_bucket(bucket)?;
        println!("bucket ready");
    }
    Ok(())
}
