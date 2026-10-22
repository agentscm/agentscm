//! Dev-only: mints synthetic users' tokens straight into a hub database (a real hub needs a
//! forge sign-in for this). Writes `NAME=SECRET` lines to the file named by its one argument,
//! created mode 0600, so secrets never reach stdout or logs. Run before `agentscm hub serve`.
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use agentscm::hub::store::{self, ForgeIdentity, mint_bound_agent_token};

const REPOSITORY: &str = "counterbranch/agentscm";
const REPOSITORY_ID: i64 = 1;
const TTL: i64 = 7 * 24 * 3600;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("AGENTSCM_DATABASE_URL")?;
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: agentscm-dev-seed-mint TOKENS_FILE")?;
    let store = store::open(&url)?;
    let mut out = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    // (env name, user id, login, agent token name, read-only)
    for (key, id, login, agent, read_only) in [
        ("IVY", 2001, "maya-chen", "claude-ivy", false),
        ("ORION", 2002, "dev-okafor", "codex-orion", false),
        ("JUNO", 2003, "sam-reyes", "claude-juno", false),
        ("PAX", 2004, "lena-ito", "gemini-pax", false),
        ("RUNE", 2005, "dev-okafor", "claude-rune", false),
        ("READ", 2001, "maya-chen", "status-page", true),
    ] {
        let identity = ForgeIdentity {
            provider: "github".into(),
            host: "github.com".into(),
            user_id: id,
        };
        let user = store.upsert_user(&identity, login)?;
        let (_, secret, _) = mint_bound_agent_token(
            &*store,
            &user.id,
            agent,
            REPOSITORY,
            REPOSITORY_ID,
            TTL,
            read_only,
        )?;
        writeln!(out, "{key}={}", secret.expose())?;
    }
    Ok(())
}
