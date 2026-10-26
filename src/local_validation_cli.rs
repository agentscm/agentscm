//! `local-validation run|show|verify`: produce, display and independently verify
//! a Git-bound local validation attestation. Keys come from `agent-task key-generate`.

use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use agentscm::evidence::{EvaluationClock, EvidenceOutcome, sign_evidence};
use agentscm::execution::Cancellation;
use agentscm::local_validation::comment::{self, envelope_sha256};
use agentscm::local_validation::run::{observe, produce};
use agentscm::local_validation::{
    Issuance, MAX_RECORD_BYTES, Observed, ValidationPolicy, ValidationRecipe, ValidationRecord,
    Verdict, VerdictState, assess, evidence_payload, select,
};
use agentscm::policy::issuers::{IssuerRegistrySnapshot, issuer_key_id};
use agentscm::{Error, Result};
use clap::{Args, Subcommand};
use serde_json::json;

use crate::agent_task_cli::{public_hex, read_signing_key, validate_private_directory_metadata};
use crate::client_cli::read_token;

const MAX_INPUT_BYTES: u64 = 2 * 1024 * 1024;
/// Comment pages (of 100) read when looking for an earlier post.
const MAX_COMMENT_PAGES: u32 = 10;
const MAX_EXPIRES_SECONDS: u64 = 366 * 24 * 3600;

#[derive(Debug, Subcommand)]
pub enum LocalValidationCommand {
    /// Run the recipe on a clean tree, sign the result and retain it before printing.
    Run(RunArgs),
    /// Print the PR-comment body for a retained attestation.
    Show {
        #[arg(long)]
        state_dir: PathBuf,
        /// Envelope SHA-256 printed by `run`.
        #[arg(long)]
        attestation: String,
    },
    /// Decide whether an attestation satisfies the exact head. Exit 0 success, 1 failure, 2 error (including unreadable inputs), 3 no attestation for the head.
    Verify(VerifyArgs),
    /// Post a retained attestation as a PR comment (GitHub). Posts no status.
    Post(PostArgs),
    /// Set the `agentscm/local-validation` status of every open pull request on `--base` from its
    /// signed attestation comments. Exit 0 when all were handled, 1 on any error or unread input,
    /// 2 when the policy is for another repository.
    Reconcile(ReconcileArgs),
}

#[derive(Debug, Args)]
pub struct ReconcileArgs {
    /// OWNER/NAME.
    #[arg(long)]
    github_repo: String,
    /// The only pull request base reconciled.
    #[arg(long, default_value = "main")]
    base: String,
    /// Directory with policy.json, recipe.json and registry.json from the default branch;
    /// relative to `--repo`, without `..`.
    #[arg(long, default_value = ".github/local-validation")]
    config_dir: PathBuf,
    /// Checkout of the default branch; its `origin` is fetched.
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    /// Name of the environment variable holding the token.
    #[arg(long, default_value = "GH_TOKEN")]
    token_env: String,
    #[arg(long, default_value = "https://api.github.com")]
    api_url: String,
    /// Link recorded on each status.
    #[arg(long)]
    target_url: Option<String>,
}

#[derive(Debug, Args)]
pub struct PostArgs {
    #[arg(long)]
    state_dir: PathBuf,
    /// Envelope SHA-256 printed by `run`.
    #[arg(long)]
    attestation: String,
    /// OWNER/NAME.
    #[arg(long)]
    github_repo: String,
    #[arg(long, value_parser = clap::value_parser!(i64).range(1..))]
    pr: i64,
    /// Absolute owner-private file holding the token.
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long, default_value = "https://api.github.com")]
    api_url: String,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long)]
    recipe: PathBuf,
    #[arg(long)]
    policy: PathBuf,
    /// IssuerRegistrySnapshot JSON.
    #[arg(long)]
    registry: PathBuf,
    /// Absolute owner-private raw Ed25519 key file (see `agent-task key-generate`).
    #[arg(long)]
    key_file: PathBuf,
    #[arg(long)]
    issuer_id: String,
    /// Absolute directory for retained attestations; created 0700.
    #[arg(long)]
    state_dir: PathBuf,
    /// Forge base branch name recorded in the context.
    #[arg(long, default_value = "main")]
    base_ref: String,
    /// Local revision resolved for the base tip and merge-base.
    #[arg(long, default_value = "origin/main")]
    base_rev: String,
    /// Defaults to the policy maximum age.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=MAX_EXPIRES_SECONDS))]
    expires_in_seconds: Option<u64>,
}

#[derive(Debug, Args)]
pub struct VerifyArgs {
    /// Comment body produced by `show`; repeat to judge a whole PR thread (order does not matter).
    #[arg(long, conflicts_with_all = ["envelope", "record"])]
    comment_body: Vec<PathBuf>,
    #[arg(long, requires = "record")]
    envelope: Option<PathBuf>,
    #[arg(long, requires = "envelope")]
    record: Option<PathBuf>,
    #[arg(long)]
    policy: PathBuf,
    /// Trusted (default-branch) recipe; its digest must equal the policy's.
    #[arg(long, default_value = ".github/local-validation/recipe.json")]
    recipe: PathBuf,
    #[arg(long)]
    registry: PathBuf,
    #[arg(long, default_value = ".")]
    repo: PathBuf,
    #[arg(long, default_value = "HEAD")]
    head: String,
    #[arg(long, default_value = "main")]
    base_ref: String,
    #[arg(long, default_value = "origin/main")]
    base_rev: String,
    #[arg(long)]
    repository: String,
}

/// Returns the process exit code.
pub async fn run(command: LocalValidationCommand) -> Result<i32> {
    match command {
        LocalValidationCommand::Run(args) => {
            // Registered before any command starts; children run in their own process groups
            // and would otherwise be orphaned by Ctrl-C.
            let mut interrupt =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            let cancel = Cancellation::default();
            let on_interrupt = cancel.clone();
            tokio::spawn(async move {
                if interrupt.recv().await.is_some() {
                    on_interrupt.cancel();
                    // A second Ctrl-C abandons the wait for the cancelled command.
                    if interrupt.recv().await.is_some() {
                        std::process::exit(130);
                    }
                }
            });
            tokio::task::spawn_blocking(move || run_recipe(args, &cancel))
                .await
                .map_err(|e| Error::Invalid(format!("run task failed: {e}")))?
        }
        LocalValidationCommand::Show {
            state_dir,
            attestation,
        } => {
            show(&state_dir, &attestation)?;
            Ok(0)
        }
        LocalValidationCommand::Verify(args) => verify(args),
        LocalValidationCommand::Post(args) => post(args).await,
        LocalValidationCommand::Reconcile(args) => reconcile(args).await,
    }
}

fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(Error::Limit(format!(
            "{} exceeds {max} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn read_registry(path: &Path) -> Result<IssuerRegistrySnapshot> {
    IssuerRegistrySnapshot::from_slice(&read_bounded(path, MAX_INPUT_BYTES)?)
}

fn now_ms() -> Option<u64> {
    let elapsed = SystemTime::now().duration_since(UNIX_EPOCH).ok()?;
    u64::try_from(elapsed.as_millis()).ok()
}

fn retain(dir: &Path, name: &str, bytes: &[u8]) -> Result<()> {
    let temporary = dir.join(format!(".{name}.tmp"));
    // A crash can leave a stale temporary; it holds nothing retained yet.
    match fs::remove_file(&temporary) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&temporary, dir.join(name))?;
    File::open(dir)?.sync_all()?;
    Ok(())
}

fn private_state_dir(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(Error::Invalid("state directory must be absolute".into()));
    }
    match DirBuilder::new().mode(0o700).create(path) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(e.into()),
        _ => {}
    }
    validate_private_directory_metadata(&fs::symlink_metadata(path)?)
}

fn run_recipe(args: RunArgs, cancel: &Cancellation) -> Result<i32> {
    let recipe: ValidationRecipe =
        ValidationRecipe::from_slice(&read_bounded(&args.recipe, MAX_INPUT_BYTES)?)?;
    let policy = ValidationPolicy::from_slice(&read_bounded(&args.policy, MAX_INPUT_BYTES)?)?;
    let registry = read_registry(&args.registry)?;
    if registry.repository != policy.repository {
        return Err(Error::Invalid(
            "registry repository differs from the policy repository".into(),
        ));
    }
    let key = read_signing_key(&args.key_file)?;
    private_state_dir(&args.state_dir)?;
    let produced = produce(
        &args.repo,
        &recipe,
        &policy,
        &args.base_ref,
        &args.base_rev,
        cancel,
    )?;
    let mut attempt = [0_u8; 16];
    getrandom::fill(&mut attempt)
        .map_err(|_| Error::Invalid("random source unavailable".into()))?;
    let attempt_id: String = attempt.iter().map(|b| format!("{b:02x}")).collect();
    let issued_at_ms = now_ms().ok_or_else(|| Error::Invalid("system clock unavailable".into()))?;
    let lifetime = args.expires_in_seconds.unwrap_or(policy.max_age_seconds);
    let key_id = issuer_key_id(&public_hex(&key))?;
    let (payload, record_bytes) = evidence_payload(
        &produced.record,
        &recipe,
        &policy,
        &Issuance {
            issuer_id: &args.issuer_id,
            key_id: &key_id,
            attempt_id: &attempt_id,
            run_generation: 1,
            issued_at_ms,
            expires_at_ms: issued_at_ms.saturating_add(lifetime.saturating_mul(1000)),
            issuer_registry_sha256: &registry.digest()?,
            dependencies: &produced.dependencies,
        },
    )?;
    let pass = payload.outcome == EvidenceOutcome::Pass;
    let envelope_bytes = serde_json::to_vec(&sign_evidence(payload, &key)?)?;
    let sha = envelope_sha256(&envelope_bytes);
    // Command output first (diagnosis for failures), then the record: an envelope on disk
    // always has its record. File names use the command position, never the recipe's ID.
    let mut output_files = Vec::new();
    for (index, output) in produced.outputs.iter().enumerate() {
        for (stream, bytes) in [("stdout", &output.stdout), ("stderr", &output.stderr)] {
            let name = format!("{sha}.cmd{}.{stream}", index + 1);
            retain(&args.state_dir, &name, bytes)?;
            output_files.push(args.state_dir.join(name).display().to_string());
        }
    }
    retain(
        &args.state_dir,
        &format!("{sha}.record.json"),
        &record_bytes,
    )?;
    retain(
        &args.state_dir,
        &format!("{sha}.envelope.json"),
        &envelope_bytes,
    )?;
    println!(
        "{}",
        json!({
            "envelope_sha256": sha,
            "outcome": if pass { "pass" } else { "fail" },
            "commit": produced.record.subject.commit,
            "output_files": output_files,
        })
    );
    Ok(i32::from(!pass))
}

fn show(state_dir: &Path, sha: &str) -> Result<()> {
    let (envelope, record) = read_retained(state_dir, sha)?;
    print!("{}", comment::render(&envelope, &record)?);
    Ok(())
}

/// The retained `(envelope, record)` bytes, checked against the envelope digest in the name.
fn read_retained(state_dir: &Path, sha: &str) -> Result<(Vec<u8>, Vec<u8>)> {
    check_sha(sha)?;
    let envelope = read_bounded(
        &state_dir.join(format!("{sha}.envelope.json")),
        MAX_RECORD_BYTES as u64,
    )?;
    let record = read_bounded(
        &state_dir.join(format!("{sha}.record.json")),
        MAX_RECORD_BYTES as u64,
    )?;
    if envelope_sha256(&envelope) != sha {
        return Err(Error::Invalid(
            "retained envelope does not match its name".into(),
        ));
    }
    Ok((envelope, record))
}

fn verdict_exit(verdict: &Verdict, commit: &str) -> i32 {
    let (name, code) = match verdict.state {
        VerdictState::Success => ("success", 0),
        VerdictState::Failure => ("failure", 1),
        VerdictState::Error => ("error", 2),
        VerdictState::Pending => ("pending", 3),
    };
    println!(
        "{}",
        json!({"state": name, "reason": verdict.reason, "detail": verdict.detail,
               "envelope_sha256": verdict.envelope_sha256, "commit": commit})
    );
    code
}

/// Input errors are an `error` verdict (exit 2), never a failure verdict (exit 1).
fn verify(args: VerifyArgs) -> Result<i32> {
    verify_inputs(args).or_else(|error| {
        let verdict = Verdict {
            state: VerdictState::Error,
            reason: "input error",
            detail: error.to_string(),
            applies_to_head: false,
            envelope_sha256: String::new(),
        };
        Ok(verdict_exit(&verdict, ""))
    })
}

fn verify_inputs(args: VerifyArgs) -> Result<i32> {
    let policy = ValidationPolicy::from_slice(&read_bounded(&args.policy, MAX_INPUT_BYTES)?)?;
    let registry = read_registry(&args.registry)?;
    let recipe = ValidationRecipe::from_slice(&read_bounded(&args.recipe, MAX_INPUT_BYTES)?)?;
    // Oversized or non-UTF-8 bodies read like any other junk: `select` ignores what does not parse.
    let bodies = args
        .comment_body
        .iter()
        .map(|path| {
            read_bounded(path, MAX_INPUT_BYTES).map(|b| String::from_utf8_lossy(&b).into_owned())
        })
        .collect::<Result<Vec<_>>>()?;
    let pair = match (&args.envelope, &args.record) {
        (Some(envelope), Some(record)) => Some((
            read_bounded(envelope, MAX_RECORD_BYTES as u64)?,
            read_bounded(record, MAX_RECORD_BYTES as u64)?,
        )),
        _ if bodies.is_empty() => {
            return Err(Error::Invalid(
                "pass --comment-body or --envelope with --record".into(),
            ));
        }
        _ => None,
    };
    let git = match observe(&args.repo, &args.head, &args.base_rev) {
        Ok(git) => git,
        Err(error) => {
            let verdict = Verdict {
                state: VerdictState::Error,
                reason: "git unavailable",
                detail: error.to_string(),
                applies_to_head: false,
                envelope_sha256: String::new(),
            };
            return Ok(verdict_exit(&verdict, ""));
        }
    };
    let clock = now_ms().map_or(EvaluationClock::Uncertain, |unix_ms| {
        EvaluationClock::Known { unix_ms }
    });
    let observed = Observed {
        repository: &args.repository,
        commit: &git.commit,
        tree: &git.tree,
        base_ref: &args.base_ref,
        merge_base: &git.merge_base,
    };
    let verdict = match pair {
        Some((envelope, record)) => assess(
            &envelope, &record, &recipe, &registry, &policy, observed, clock,
        ),
        None => {
            let refs: Vec<&str> = bodies.iter().map(String::as_str).collect();
            select(&refs, &recipe, &registry, &policy, observed, clock)
        }
    };
    Ok(verdict_exit(&verdict, &git.commit))
}

fn check_sha(sha: &str) -> Result<()> {
    if sha.len() != 64 || !sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
        return Err(Error::Invalid(
            "attestation must be a lowercase SHA-256".into(),
        ));
    }
    Ok(())
}

fn check_github_repo(repo: &str) -> Result<()> {
    let valid_name = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    if !repo.split('/').all(valid_name) || repo.matches('/').count() != 1 {
        return Err(Error::Invalid("--github-repo must be OWNER/NAME".into()));
    }
    Ok(())
}

async fn reconcile(args: ReconcileArgs) -> Result<i32> {
    use agentscm::hub::forge_http::check_base_url;
    use agentscm::hub::github::{Api, CommitStatus, StatusState, valid_ref_name};
    use agentscm::local_validation::reconcile::Context;
    check_github_repo(&args.github_repo)?;
    if !valid_ref_name(&args.base) {
        return Err(Error::Invalid("--base is not a branch name".into()));
    }
    let token = std::env::var(&args.token_env)
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::Invalid("the token environment variable is unset or empty".into()))?;
    let Some(config_dir) = args.config_dir.to_str().filter(|d| {
        !d.is_empty()
            && !d.starts_with('-')
            && args
                .config_dir
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
    }) else {
        eprintln!("--config-dir must be a relative path inside --repo; nothing written");
        return Ok(2);
    };
    let target_ok = args.target_url.as_deref().is_none_or(|url| {
        CommitStatus::new(&"0".repeat(40), StatusState::Pending, "x", "x")
            .and_then(|status| status.with_target_url(url))
            .is_ok()
    });
    if !target_ok {
        eprintln!(
            "--target-url must be a short http(s) URL without control characters; nothing written"
        );
        return Ok(2);
    }
    // The guard compares `HEAD`, so the files read must be the committed ones.
    let committed = agentscm::local_validation::run::config_dir_tree(&args.repo, config_dir)
        .map(|_| ())
        .and_then(|()| agentscm::local_validation::run::config_dir_dirty(&args.repo, config_dir))
        .map_err(|e| {
            eprintln!("--config-dir cannot be read at HEAD of --repo ({e:?}); nothing written");
        });
    match committed {
        Ok(false) => {}
        Ok(true) => {
            eprintln!("--config-dir has uncommitted changes; nothing written");
            return Ok(2);
        }
        Err(()) => return Ok(2),
    }
    let read = |name: &str| {
        read_bounded(
            &args.repo.join(&args.config_dir).join(name),
            MAX_INPUT_BYTES,
        )
    };
    let policy = ValidationPolicy::from_slice(&read("policy.json")?)?;
    let recipe = ValidationRecipe::from_slice(&read("recipe.json")?)?;
    let registry = IssuerRegistrySnapshot::from_slice(&read("registry.json")?)?;
    if policy.repository != format!("github.com/{}", args.github_repo) {
        eprintln!("policy repository does not match --github-repo; nothing written");
        return Ok(2);
    }
    if registry.repository != policy.repository {
        return Err(Error::Invalid(
            "registry repository differs from the policy repository".into(),
        ));
    }
    let base_url = check_base_url(&args.api_url).map_err(|e| Error::Invalid(e.to_string()))?;
    let api = Api::new(&base_url, &base_url).map_err(|e| Error::Invalid(e.to_string()))?;
    Ok(Context {
        api: &api,
        token: &token,
        github_repo: &args.github_repo,
        base: &args.base,
        config_dir,
        git_repo: &args.repo,
        repository: &policy.repository,
        policy: &policy,
        recipe: &recipe,
        registry: &registry,
        target_url: args.target_url.as_deref(),
    }
    .reconcile()
    .await)
}

/// Posts the retained attestation as one PR comment, adopting this login's earlier comment for
/// the same envelope so a lost create reply cannot duplicate it. The local producer never posts
/// a status; the separate verifier does.
async fn post(args: PostArgs) -> Result<i32> {
    use agentscm::hub::forge_http::check_base_url;
    use agentscm::hub::github::Api;
    check_sha(&args.attestation)?;
    check_github_repo(&args.github_repo)?;
    if !args.token_file.is_absolute() {
        return Err(Error::Invalid("--token-file must be absolute".into()));
    }
    let token = read_token(&args.token_file)?;
    let (envelope, record) = read_retained(&args.state_dir, &args.attestation)?;
    let body = comment::render(&envelope, &record)?;
    let marker = body.lines().next().unwrap_or_default();
    let commit = ValidationRecord::from_slice(&record)?.subject.commit;
    let base = check_base_url(&args.api_url).map_err(|e| Error::Invalid(e.to_string()))?;
    let api = Api::new(&base, &base).map_err(|e| Error::Invalid(e.to_string()))?;
    let (repo, pr) = (args.github_repo.as_str(), args.pr);
    let gh = |e: agentscm::hub::github::GhError| Error::Invalid(format!("GitHub: {e:?}"));
    let pull = api
        .pull(&token, repo, pr)
        .await
        .map_err(gh)?
        .ok_or_else(|| Error::Invalid("pull request not found".into()))?;
    if pull["head"]["sha"].as_str() != Some(commit.as_str()) {
        return Err(Error::Conflict(
            "pull request head differs from the attestation commit; nothing posted".into(),
        ));
    }
    let (_, login) = api.user(&token).await.map_err(gh)?;
    let (comments, truncated) = api
        .comments_pages(&token, repo, pr, MAX_COMMENT_PAGES)
        .await
        .map_err(gh)?;
    let own = comments.into_iter().find(|(_, author, text)| {
        author.eq_ignore_ascii_case(&login)
            && text.lines().next().map(str::trim_end) == Some(marker)
    });
    if own.is_none() && truncated {
        return Err(Error::Limit(
            "too many comments to rule out an earlier post; nothing posted".into(),
        ));
    }
    let adopted = match own {
        Some((id, _, text)) if text == body => Some(id),
        Some((id, _, _)) => api
            .edit_comment(&token, repo, id, &body)
            .await
            .map_err(gh)?
            .then_some(id),
        None => None,
    };
    let id = match adopted {
        Some(id) => id,
        None => api
            .create_comment(&token, repo, pr, &body)
            .await
            .map_err(gh)?,
    };
    let receipt = json!({
        "github_repo": repo,
        "pr": pr,
        "comment_id": id,
        "html_url": format!("{}#issuecomment-{id}", pull["html_url"].as_str().unwrap_or_default()),
    });
    retain(
        &args.state_dir,
        &format!("{}.posted.json", args.attestation),
        &serde_json::to_vec(&receipt)?,
    )?;
    println!("{receipt}");
    Ok(0)
}
