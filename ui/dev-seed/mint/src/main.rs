//! Dev-only: mints synthetic users' tokens straight into a hub database (a real hub needs a
//! forge sign-in for this). Writes `NAME=SECRET` lines to the file named by its one argument,
//! created mode 0600, so secrets never reach stdout or logs. Run before `agentscm hub serve`.
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

use agentscm::hub::connection::NewConnection;
use agentscm::hub::external_runs::{ExternalKind, ExternalOutcome, ExternalRun};
use agentscm::hub::publish::{Change, Manifest, NewPublication, Person, Update, derive_id};
use agentscm::hub::store::{self, ForgeIdentity, Store, mint_bound_agent_token, new_connection_id};

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
    let mut ivy = String::new();
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
        let (token_id, secret, _) = mint_bound_agent_token(
            &*store,
            &user.id,
            agent,
            REPOSITORY,
            REPOSITORY_ID,
            TTL,
            read_only,
        )?;
        writeln!(out, "{key}={}", secret.expose())?;
        if key == "IVY" {
            ivy = token_id;
        }
    }
    seed_publications(&*store, &ivy)
}

/// Fake lowercase hex of `len` chars, stable per `seed`.
fn hex(seed: u64, len: usize) -> String {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut out = String::new();
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push_str(&format!("{x:016x}"));
    }
    out.truncate(len);
    out
}

/// Publications in every state, with external CI and deployment outcomes on the merged ones.
/// The hub reads those only for a GitHub connection holding the repository, and
/// `AGENTSCM_GITHUB_EXTERNAL_RUNS` naming it (start.sh sets that). The connection never polls.
fn seed_publications(store: &dyn Store, token: &str) -> Result<(), Box<dyn std::error::Error>> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs() as i64;
    let conn = store.insert_connection(
        &NewConnection {
            id: new_connection_id()?,
            provider: "github".into(),
            host: "github.com".into(),
            kind: "pat".into(),
            app_id: None,
            app_slug: None,
            acts_as: Some("dev-seed".into()),
        },
        "dev-seed",
    )?;
    let repo = store.upsert_repository(
        &conn.id,
        "github.com",
        REPOSITORY_ID,
        REPOSITORY,
        None,
        None,
    )?;
    store.set_next_poll(&conn.id, now + 10 * 365 * 24 * 3600)?;

    let web = format!("https://github.com/{REPOSITORY}");
    // (event, branch, title, final step: 0 pending, 1 rejected, 2 branch, 3 PR, 4+ merged)
    let specs = [
        (
            101,
            "agent/ivy/retry-backoff-jitter",
            "Add jitter to hub retry backoff",
            0,
        ),
        (
            102,
            "agent/orion/rename-config-keys",
            "Rename legacy config keys",
            1,
        ),
        (
            103,
            "agent/juno/status-page-empty-states",
            "Status page: empty states",
            2,
        ),
        (
            104,
            "agent/pax/activity-pagination",
            "Paginate the activity feed",
            3,
        ),
        (
            105,
            "agent/ivy/handoff-expiry-sweep",
            "Sweep expired handoffs",
            4,
        ),
        (
            106,
            "agent/rune/schema-migration-lock",
            "Lock schema migrations",
            5,
        ),
        (
            107,
            "agent/orion/token-ttl-docs",
            "Document token lifetimes",
            6,
        ),
    ];
    let mut pr = 480;
    for (n, (event, branch, title, step)) in specs.into_iter().enumerate() {
        let seed = event * 7;
        let (commit, tree, parent) = (hex(seed, 40), hex(seed + 1, 40), hex(seed + 2, 40));
        let when = now - 3600 * (specs.len() - n) as i64;
        let person = |name: &str, email: &str| Person {
            name: name.into(),
            email: email.into(),
            date: "2026-10-22T09:00:00Z".into(),
        };
        let blob = hex(seed + 3, 40);
        let manifest = Manifest {
            revision: hex(seed + 4, 64),
            event_seq: Some(event as i64),
            candidate: None,
            commit: commit.clone(),
            tree,
            parent,
            branch: branch.into(),
            base: "main".into(),
            title: title.into(),
            body: format!("{title}.\n\nSynthetic dev-seed publication."),
            author: person("Claude Ivy", "ivy@example.test"),
            committer: person("Claude Ivy", "ivy@example.test"),
            message: format!("{title}\n"),
            changes: vec![Change {
                path: "src/lib.rs".into(),
                mode: "100644".into(),
                blob: Some(blob),
                sha256: Some(hex(seed + 5, 64)),
                size: 1200,
            }],
        };
        let id = derive_id(REPOSITORY, event, &commit, branch);
        store.create_publication(
            token,
            &NewPublication {
                id: id.clone(),
                manifest,
            },
            when,
        )?;
        let up = |u: Update, dt: i64| store.update_publication(&id, &u, when + dt);
        match step {
            0 => {}
            1 => {
                up(Update::BranchAttempt, 10)?;
                up(
                    Update::BranchRejected {
                        status: 422,
                        message: "Reference already exists".into(),
                    },
                    20,
                )?;
            }
            _ => {
                up(Update::BranchAttempt, 10)?;
                up(Update::BranchCreated, 20)?;
                if step >= 3 {
                    pr += 1;
                    up(
                        Update::PullRequest {
                            number: pr,
                            node_id: format!("PR_kwDO{pr:08}"),
                            html_url: format!("{web}/pull/{pr}"),
                        },
                        60,
                    )?;
                }
                if step >= 4 {
                    let merge = hex(seed + 6, 40);
                    up(
                        Update::Merged {
                            merge_commit: merge.clone(),
                        },
                        600,
                    )?;
                    external(
                        store,
                        &repo.id,
                        &web,
                        step,
                        &merge,
                        branch,
                        when + 700,
                        seed as i64,
                    )?;
                }
            }
        }
        println!("publication {id} {branch}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn external(
    store: &dyn Store,
    repo: &str,
    web: &str,
    step: i32,
    sha: &str,
    branch: &str,
    at: i64,
    base: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    use ExternalKind::{Ci, Deployment};
    use ExternalOutcome::{Failed, Running, Succeeded};
    // (kind, name, environment, outcome)
    let runs: &[(ExternalKind, &str, Option<&str>, ExternalOutcome)] = match step {
        4 => &[
            (Ci, ".github/workflows/ci.yml", None, Succeeded),
            (Deployment, "Deploy staging", Some("staging"), Succeeded),
            (Deployment, "Deploy production", Some("production"), Running),
        ],
        5 => &[
            (Ci, ".github/workflows/ci.yml", None, Failed),
            (Ci, ".github/workflows/e2e.yml", None, Running),
        ],
        _ => &[],
    };
    for (i, (kind, name, environment, outcome)) in runs.iter().enumerate() {
        let run_id = 9_000_000_000 + base * 10 + i as i64;
        let path = if *kind == Ci {
            "actions/runs"
        } else {
            "deployments"
        };
        store.observe_external_run(
            repo,
            &ExternalRun {
                provider: "github".into(),
                kind: *kind,
                run_id,
                attempt: 1,
                commit_sha: sha.into(),
                branch: (*kind == Ci).then(|| branch.into()),
                environment: environment.map(Into::into),
                name: (*name).into(),
                outcome: *outcome,
                provider_updated_at: at + 60 * i as i64,
                html_url: Some(format!("{web}/{path}/{run_id}")),
            },
        )?;
    }
    // Step 6 stays unobserved: no marker.
    if step <= 5 {
        store.mark_external_reconciled(repo, sha, at + 300, step == 4)?;
    }
    Ok(())
}
