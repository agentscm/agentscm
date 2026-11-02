//! Explicit operator-local installation, verified forge observation and outbox pickup.
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentscm::hub::connection::ConnectedRepository;
use agentscm::hub::forge_api::ForgeApi;
use agentscm::hub::pipeline_rules::{
    self, ConfirmedMerge, MergeTrigger, PipelineRecipe, PipelineRule, RuleJournal,
};
use agentscm::{Error, Result};
use clap::Subcommand;
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Subcommand)]
pub enum PipelineCommand {
    /// Install an operator-local rule from a strict JSON manifest; captures local Git only.
    Install { input: PathBuf },
    /// Replace one rule with an expected current generation; stable ID prevents replay.
    Update {
        input: PathBuf,
        #[arg(long)]
        expected_generation: i64,
    },
    /// Enable or disable one installed rule with compare-and-swap.
    SetEnabled {
        repository: String,
        rule_id: String,
        #[arg(long)]
        expected_generation: i64,
        #[arg(long,action=clap::ArgAction::Set)]
        enabled: bool,
    },
    /// Inspect installed local rules and retained outbox states.
    Status {
        #[arg(long)]
        dispatch: Option<String>,
        #[arg(long, conflicts_with = "dispatch")]
        before: Option<i64>,
    },
    /// Match a synthetic event without accepting it. This does not verify forge facts.
    Preview {
        repository: String,
        rule_id: String,
        event: PathBuf,
    },
    /// Verify a numbered PR/MR through the configured forge and accept matching rules.
    Observe {
        #[arg(long)]
        connection: PathBuf,
        #[arg(long)]
        proposal: i64,
        #[arg(long)]
        delivery_id: Option<String>,
    },
    /// Discover recent closed/merged proposals and verify each candidate before accepting it.
    Poll {
        #[arg(long)]
        connection: PathBuf,
    },
    /// Run pending work once. Never fetches source or retries interrupted actions.
    Pickup,
    /// Requeue a blocked/interrupted outbox item; adopts the same workflow attempt journal.
    Recover {
        dispatch_id: String,
        #[arg(long)]
        expected_generation: i64,
    },
    /// Opt-in foreground observer/runner loop. Run under an operator service for background use.
    Watch {
        #[arg(long)]
        connection: Option<PathBuf>,
        #[arg(long,default_value_t=300,value_parser=clap::value_parser!(u64).range(5..=3600))]
        interval_seconds: u64,
        #[arg(long)]
        once: bool,
    },
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallManifest {
    repository: String,
    provider: String,
    host: String,
    provider_repository_id: i64,
    rule_id: String,
    enabled: bool,
    trigger: MergeTrigger,
    recipe: PipelineRecipe,
    profile_path: PathBuf,
    git: PathBuf,
    git_repository: PathBuf,
    /// Full existing local Git object, used only to pin the reviewed skill resources.
    source_commit: String,
}

/// Local operator connection, never read from candidate source. Credentials stay in an
/// explicitly named environment variable and are never retained in rule/event records.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LocalConnection {
    pub(crate) provider: String,
    pub(crate) host: String,
    pub(crate) api_url: String,
    pub(crate) web_url: String,
    pub(crate) repository: String,
    pub(crate) provider_repository_id: i64,
    pub(crate) token_env: String,
}
impl LocalConnection {
    pub(crate) fn validate(&self) -> Result<()> {
        if !matches!(self.provider.as_str(), "github" | "gitlab")
            || self.provider_repository_id < 1
            || self.repository.is_empty()
            || self
                .repository
                .split('/')
                .any(|v| v.is_empty() || v == "." || v == ".." || v.contains(['?', '#', '\\', '%']))
            || self.token_env.is_empty()
            || !self
                .token_env
                .bytes()
                .all(|v| v.is_ascii_alphanumeric() || v == b'_')
        {
            return Err(Error::Invalid("invalid operator forge connection".into()));
        }
        for (endpoint, is_api) in [(&self.api_url, true), (&self.web_url, false)] {
            let url = reqwest::Url::parse(endpoint)
                .map_err(|_| Error::Invalid("invalid forge endpoint".into()))?;
            let host = url
                .host_str()
                .ok_or_else(|| Error::Invalid("forge endpoint needs a host".into()))?;
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.scheme() != "https"
                || (!is_api && host != self.host)
                || (is_api
                    && host != self.host
                    && !(self.provider == "github"
                        && self.host == "github.com"
                        && host == "api.github.com"))
            {
                return Err(Error::Invalid(
                    "forge endpoints must use HTTPS and the configured host".into(),
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn repository(&self) -> ConnectedRepository {
        ConnectedRepository {
            id: "operator-local".into(),
            connection: "operator-local".into(),
            host: self.host.clone(),
            repository_id: self.provider_repository_id,
            full_name: self.repository.clone(),
            node_id: None,
            installation_id: None,
            pulls_etag: None,
        }
    }
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::Invalid(
            "operator input must be a regular file".into(),
        ));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(256 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 256 * 1024 {
        return Err(Error::Limit("operator input exceeds 256 KiB".into()));
    }
    Ok(serde_json::from_slice(&bytes)?)
}
pub(crate) fn root(data: &Path) -> Result<PathBuf> {
    let root = data.join("pipeline-rules");
    if !data.join("state.sqlite").is_file() {
        return Err(Error::Invalid(
            "pipeline commands require initialized operator data; run agentscm init first".into(),
        ));
    }
    if root.exists() {
        let meta = std::fs::symlink_metadata(&root)?;
        if !meta.is_dir() || meta.file_type().is_symlink() || meta.permissions().mode() & 0o077 != 0
        {
            return Err(Error::Invalid(
                "pipeline state requires a private operator directory".into(),
            ));
        }
    } else {
        std::fs::DirBuilder::new().recursive(false).create(&root)?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(root)
}
pub(crate) fn runner_lease(root: &Path) -> Result<agentscm::lease::Lease> {
    let path = root.join("runner.lock");
    if path.exists() && std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
        return Err(Error::Invalid("runner lease cannot be a symlink".into()));
    }
    agentscm::lease::Lease::try_exclusive(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?,
    )
    .map_err(|_| Error::Conflict("another local pipeline runner is active".into()))
}
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn install(
    data: &Path,
    journal: &mut RuleJournal,
    input: &Path,
    expected: Option<i64>,
) -> Result<Value> {
    let manifest: InstallManifest = read_json(input)?;
    let profile_path = manifest.profile_path.canonicalize()?;
    let git_repository = manifest.git_repository.canonicalize()?;
    if profile_path.starts_with(&git_repository) {
        return Err(Error::Invalid(
            "operator profile must be outside candidate Git source".into(),
        ));
    }
    let profile = pipeline_rules::read_profile_configuration(&profile_path)?;
    let exclusions = pipeline_rules::executable_exclusions(data, &git_repository)?;
    let excluded: Vec<&Path> = exclusions.iter().map(PathBuf::as_path).collect();
    let git_identity = agentscm::execution::executable::capture(&manifest.git, &excluded)?;
    profile.executable_identities(&excluded)?;
    let mut rule = PipelineRule {
        repository: manifest.repository,
        provider: manifest.provider,
        host: manifest.host,
        provider_repository_id: manifest.provider_repository_id,
        rule_id: manifest.rule_id,
        generation: expected
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| Error::Limit("rule generation exhausted".into()))?,
        enabled: manifest.enabled,
        trigger: manifest.trigger,
        recipe_digest: manifest.recipe.digest()?,
        recipe: manifest.recipe,
        skill_digest: "install-preflight".into(),
        profile_id: profile.id.clone(),
        profile_digest: profile.digest()?,
        profile_path,
        authority_ceiling: "productive_local".into(),
        git: git_identity.path.clone(),
        git_identity,
        git_repository,
    };
    journal.bind_repository(&rule)?;
    let plan = pipeline_rules::compile_source(data, &rule, &manifest.source_commit)?;
    rule.skill_digest = plan.skill_digest().into();
    journal.install(&rule, expected)?;
    Ok(
        json!({"rule":rule,"source_commit":manifest.source_commit,"note":"exact resource identity pinned; local commands use operator privileges"}),
    )
}

pub(crate) async fn confirm(config_path: &Path, proposal: i64) -> Result<ConfirmedMerge> {
    let config: LocalConnection = read_json(config_path)?;
    config.validate()?;
    let token = std::env::var(&config.token_env)
        .map_err(|_| Error::Invalid("configured forge credential is unavailable".into()))?;
    if token.is_empty() {
        return Err(Error::Invalid(
            "configured forge credential is empty".into(),
        ));
    }
    let github;
    let gitlab;
    let api = if config.provider == "github" {
        github = agentscm::hub::github::Api::new(&config.api_url, &config.web_url)
            .map_err(|_| Error::Invalid("forge client configuration".into()))?;
        ForgeApi::Github(&github)
    } else {
        gitlab = agentscm::hub::gitlab::Api::new(&config.api_url)
            .map_err(|_| Error::Invalid("forge client configuration".into()))?;
        ForgeApi::Gitlab(&gitlab)
    };
    pipeline_rules::confirm_merge(api, &token, &config.repository(), proposal, now(), None)
        .await?
        .ok_or_else(|| Error::Conflict("proposal is not confirmed merged".into()))
}

async fn observe_one(
    api: ForgeApi<'_>,
    token: &str,
    config: &LocalConnection,
    journal: &mut RuleJournal,
    number: i64,
    delivery: Option<String>,
) -> Result<Value> {
    let event =
        pipeline_rules::confirm_merge(api, token, &config.repository(), number, now(), delivery)
            .await?;
    let Some(event) = event else {
        return Ok(json!({"accepted":[],"reason":"proposal not confirmed merged"}));
    };
    let previews: Vec<_> = journal
        .rules()?
        .iter()
        .filter(|r| r.repository == config.repository)
        .map(|rule| json!({"rule_id":rule.rule_id,"preview":pipeline_rules::preview(rule,&event)}))
        .collect();
    let accepted = journal.accept(&event)?;
    Ok(
        json!({"event":event,"accepted":accepted,"previews":previews,"labels_basis":"observation_time"}),
    )
}
async fn observe(
    config_path: &Path,
    journal: &mut RuleJournal,
    proposal: Option<i64>,
    delivery: Option<String>,
) -> Result<Value> {
    let config: LocalConnection = read_json(config_path)?;
    config.validate()?;
    journal.assert_repository(
        &config.provider,
        &config.host,
        config.provider_repository_id,
    )?;
    let token = std::env::var(&config.token_env)
        .map_err(|_| Error::Invalid("configured forge credential is unavailable".into()))?;
    if token.is_empty() {
        return Err(Error::Invalid(
            "configured forge credential is empty".into(),
        ));
    }
    let github;
    let gitlab;
    let api = if config.provider == "github" {
        github = agentscm::hub::github::Api::new(&config.api_url, &config.web_url)
            .map_err(|_| Error::Invalid("forge client configuration".into()))?;
        ForgeApi::Github(&github)
    } else {
        gitlab = agentscm::hub::gitlab::Api::new(&config.api_url)
            .map_err(|_| Error::Invalid("forge client configuration".into()))?;
        ForgeApi::Gitlab(&gitlab)
    };
    if let Some(number) = proposal {
        return observe_one(api, &token, &config, journal, number, delivery).await;
    }
    Ok(serde_json::to_value(
        pipeline_rules::poll_merges(api, &token, &config.repository(), journal, now()).await?,
    )?)
}

fn pickup(
    data: &Path,
    root: &Path,
    journal: &mut RuleJournal,
    cancellation: &agentscm::execution::Cancellation,
) -> Result<Value> {
    let work = root.join("work");
    if !work.exists() {
        std::fs::DirBuilder::new().create(&work)?;
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut results = Vec::new();
    for dispatch in journal.pending_legacy(32)? {
        if cancellation.is_cancelled() {
            break;
        }
        let Some(claim) = journal.claim_legacy(&dispatch.dispatch_id)? else {
            continue;
        };
        results.push(
            match pipeline_rules::deliver_with_cancellation(
                data,
                journal,
                &claim,
                &work,
                cancellation,
            ) {
                Ok(run) => json!({"dispatch_id":dispatch.dispatch_id,"run":run}),
                Err(error) => {
                    json!({"dispatch_id":dispatch.dispatch_id,"blocked":error.to_string()})
                }
            },
        );
    }
    Ok(json!({"deliveries":results}))
}

pub async fn run(data: &Path, command: PipelineCommand) -> Result<Value> {
    let root = root(data)?;
    let mut journal = RuleJournal::open(&root.join("state.sqlite"))?;
    match command {
        PipelineCommand::Install { input } => install(data, &mut journal, &input, None),
        PipelineCommand::Update {
            input,
            expected_generation,
        } => install(data, &mut journal, &input, Some(expected_generation)),
        PipelineCommand::SetEnabled {
            repository,
            rule_id,
            expected_generation,
            enabled,
        } => {
            let mut rule = journal.rule(&repository, &rule_id)?;
            rule.enabled = enabled;
            rule.generation = expected_generation
                .checked_add(1)
                .ok_or_else(|| Error::Limit("rule generation exhausted".into()))?;
            journal.install(&rule, Some(expected_generation))?;
            Ok(json!({"rule":rule}))
        }
        PipelineCommand::Status { dispatch, before } => {
            if let Some(id) = dispatch {
                return Ok(json!({"dispatch":journal.dispatch(&id)?}));
            }
            let dispatches = journal.dispatches_page(256, before)?;
            let next_before = if dispatches.len() == 256 {
                dispatches.last().map(|v| v.sequence)
            } else {
                None
            };
            Ok(
                json!({"rules":journal.rules()?,"dispatches":dispatches,"next_before":next_before,"polls":journal.polls()?}),
            )
        }
        PipelineCommand::Preview {
            repository,
            rule_id,
            event,
        } => {
            let rule = journal.rule(&repository, &rule_id)?;
            let event: ConfirmedMerge = read_json(&event)?;
            Ok(json!({"verified":false,"preview":pipeline_rules::preview(&rule,&event)}))
        }
        PipelineCommand::Observe {
            connection,
            proposal,
            delivery_id,
        } => observe(&connection, &mut journal, Some(proposal), delivery_id).await,
        PipelineCommand::Poll { connection } => {
            observe(&connection, &mut journal, None, None).await
        }
        PipelineCommand::Pickup => {
            let _lease = runner_lease(&root)?;
            let signals = crate::skill_workflow_cli::ProductiveSignals::new()?;
            tokio::task::block_in_place(|| pickup(data, &root, &mut journal, &signals.cancellation))
        }
        PipelineCommand::Recover {
            dispatch_id,
            expected_generation,
        } => {
            let _lease = runner_lease(&root)?;
            journal.recover_legacy(&dispatch_id, expected_generation)?;
            Ok(
                json!({"dispatch_id":dispatch_id,"state":"pending","run_identity":"retained; interrupted attempts are never replayed"}),
            )
        }
        PipelineCommand::Watch {
            connection,
            interval_seconds,
            once,
        } => {
            let _lease = runner_lease(&root)?;
            let mut signals = crate::skill_workflow_cli::ProductiveSignals::new()?;
            let mut deferred: Option<(Option<i64>, Value)> = None;
            loop {
                if signals.cancellation.is_cancelled() {
                    return Ok(json!({"stopped":true}));
                }
                let observed = if let Some((until, blocked)) = &deferred {
                    if until.is_none_or(|at| now() / 1000 < at) {
                        blocked.clone()
                    } else {
                        deferred = None;
                        json!(null)
                    }
                } else {
                    json!(null)
                };
                let observed = if !observed.is_null() {
                    observed
                } else if let Some(path) = &connection {
                    tokio::select! {
                        result=observe(path,&mut journal,None,None)=>match result {Ok(v)=>v,Err(e)=>json!({"blocked":e.to_string()})},
                        _=signals.stopped()=>return Ok(json!({"stopped":true})),
                    }
                } else {
                    json!(null)
                };
                if let Some(kind) = observed.pointer("/blocked/kind").and_then(Value::as_str)
                    && matches!(kind, "authentication" | "rate_limited")
                {
                    let retry = observed
                        .pointer("/blocked/retry_at")
                        .and_then(Value::as_i64)
                        .filter(|at| *at >= 0 && *at <= now() / 1000 + 86400);
                    deferred = Some((retry, observed.clone()));
                }
                let picked = tokio::task::block_in_place(|| {
                    pickup(data, &root, &mut journal, &signals.cancellation)
                })?;
                let result = json!({"observed":observed,"pickup":picked,"stopped":signals.cancellation.is_cancelled()});
                if once || signals.cancellation.is_cancelled() {
                    return Ok(result);
                }
                println!("{}", serde_json::to_string(&result)?);
                tokio::select! {_=tokio::time::sleep(Duration::from_secs(interval_seconds))=>{},_=signals.stopped()=>return Ok(json!({"stopped":true}))}
            }
        }
    }
}
