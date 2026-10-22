//! GitHub coordination sync: connections, a poller and webhooks that mirror open
//! pull requests and recent branches into the hub, merge them with declared
//! intents, and (when a connection opts in) write one comment and one status.
//! Everything read from GitHub is stored as an observation with exact ids.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::IpAddr;
use std::path::{Path as FsPath, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Json, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{OwnedMutexGuard, Semaphore};

use super::connection::{ConnectedRepository, Connection, NewConnection, Observation, Observed};
use super::forge_api::ForgeApi;
use super::forge_http::check_base_url;
use super::github::{
    Api, AppKey, Branch, Gh, GhError, MAX_RECENT_BRANCHES, header_str, valid_ref_name,
    webhook_signature_ok,
};
use super::gitlab;
use super::http::{ApiError, JsonBody, Reader, blocking};
use super::intent::{Intent, MAX_LIVE_INTENTS, NOTICES, key_of, overlaps, scope};
use super::login::{HumanSession, Table, forge_urls, optional_secret, random_hex};
use super::store::{Store, lower_hex_of, new_connection_id, unix_now, validate_repository};
use super::{HubError, HubState, Result};
use crate::hmac::{constant_time_eq, hmac_sha256};

const WEBHOOK_BODY_BYTES: usize = 1 << 20;
const SETUP_SECONDS: i64 = 600;
/// The recorded actor of the connection a first-run setup creates: no user exists yet.
const BOOTSTRAP_ACTOR: &str = "bootstrap";
const BOOTSTRAP_FILE: &str = "bootstrap-setup-url";
const MAX_JOBS: usize = 4;
const MAX_NAMED_BRANCHES: usize = 50;
const MAX_REREADS: usize = 100;
/// Webhook jobs queued or running; past it a delivery is recorded and left to the poller.
const MAX_PENDING_WEBHOOKS: usize = 256;
const MAX_NOTICED: usize = 10_000;
const TOMBSTONE_KEEP_MS: i64 = 24 * 60 * 60 * 1000;
/// An installation token is renewed when it has less than this left.
const TOKEN_SLACK_SECONDS: i64 = 300;
const MAX_CREDENTIAL_FILE_BYTES: u64 = 64 * 1024;
const MARKER_PREFIX: &str = "agentscm-hub";
/// Longest task text copied into a comment.
const COMMENT_TASK_CHARS: usize = 200;
const COMMENT_PATHS: usize = 10;

pub struct SyncConfig {
    /// The GitHub web URL, without a trailing slash.
    pub web: String,
    pub api: String,
    /// The identity host of connections: the web URL's host.
    pub host: String,
    /// Where GitHub reaches this hub (webhooks and the manifest redirect).
    pub public_url: String,
    pub secrets: PathBuf,
    pub activity_days: i64,
    /// Lowercase `owner/name` of the GitHub repositories whose external runs are read.
    external_runs: BTreeSet<String>,
    pat: Option<Pat>,
    gitlab: Option<GitlabConfig>,
}

/// A GitLab token connection: the forge's URLs and the credential.
struct GitlabConfig {
    api: String,
    host: String,
    token: Pat,
}

struct Pat {
    token: String,
    webhook_secret: Option<String>,
}

impl SyncConfig {
    /// Reads `AGENTSCM_GITHUB_URL`, `_API_URL`, `AGENTSCM_GITHUB_PAT[_FILE]` and
    /// `AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET[_FILE]`, and `AGENTSCM_GITLAB_URL`,
    /// `AGENTSCM_GITLAB_TOKEN[_FILE]`, `AGENTSCM_GITLAB_WEBHOOK_SECRET[_FILE]` and
    /// `AGENTSCM_GITHUB_EXTERNAL_RUNS` (comma-separated `owner/name`) through `lookup`.
    pub fn from_lookup(
        public_url: &str,
        secrets: PathBuf,
        activity_days: i64,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Self> {
        // Blank means unset (compose passes unset variables as ""); a GitLab connection without a
        // webhook secret refuses every delivery, so this never opens the webhook.
        let lookup = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let urls = forge_urls("github", "GITHUB", "https://github.com", &lookup)?;
        let token = optional_secret(&lookup, "AGENTSCM_GITHUB_PAT_FILE", "AGENTSCM_GITHUB_PAT")?;
        let hook = optional_secret(
            &lookup,
            "AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET_FILE",
            "AGENTSCM_GITHUB_PAT_WEBHOOK_SECRET",
        )?;
        if hook.is_some() && token.is_none() {
            return Err(HubError::Invalid(
                "a PAT webhook secret needs AGENTSCM_GITHUB_PAT or AGENTSCM_GITHUB_PAT_FILE".into(),
            ));
        }
        let gitlab_token = optional_secret(
            &lookup,
            "AGENTSCM_GITLAB_TOKEN_FILE",
            "AGENTSCM_GITLAB_TOKEN",
        )?;
        let gitlab_hook = optional_secret(
            &lookup,
            "AGENTSCM_GITLAB_WEBHOOK_SECRET_FILE",
            "AGENTSCM_GITLAB_WEBHOOK_SECRET",
        )?;
        let gitlab = match gitlab_token {
            Some(token) => {
                let gl = forge_urls("gitlab", "GITLAB", "https://gitlab.com", &lookup)?;
                // Stores key repositories and PAT connections by host alone.
                if gl.host == urls.host {
                    return Err(HubError::Invalid(
                        "GitHub and GitLab must be on different hosts".into(),
                    ));
                }
                if gitlab_hook.is_none() {
                    eprintln!(
                        "hub: no AGENTSCM_GITLAB_WEBHOOK_SECRET; GitLab webhooks are refused and only polling runs"
                    );
                }
                Some(GitlabConfig {
                    api: gl.api,
                    host: gl.host,
                    token: Pat {
                        token,
                        webhook_secret: gitlab_hook,
                    },
                })
            }
            None if gitlab_hook.is_some() => {
                return Err(HubError::Invalid(
                    "a GitLab webhook secret needs AGENTSCM_GITLAB_TOKEN or AGENTSCM_GITLAB_TOKEN_FILE"
                        .into(),
                ));
            }
            None => None,
        };
        if !(1..=365).contains(&activity_days) {
            return Err(HubError::Invalid("--activity-days must be 1 to 365".into()));
        }
        let external_runs = lookup("AGENTSCM_GITHUB_EXTERNAL_RUNS")
            .map(|list| {
                list.split(',')
                    .map(|name| {
                        let name = name.trim();
                        if name.split('/').count() != 2 || validate_repository(name).is_err() {
                            return Err(HubError::Invalid(
                                "AGENTSCM_GITHUB_EXTERNAL_RUNS must list owner/name repositories"
                                    .into(),
                            ));
                        }
                        Ok(name.to_ascii_lowercase())
                    })
                    .collect::<Result<BTreeSet<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            web: urls.web,
            api: urls.api,
            host: urls.host,
            public_url: public_url.to_owned(),
            secrets,
            activity_days,
            external_runs,
            pat: token.map(|token| Pat {
                token,
                webhook_secret: hook,
            }),
            gitlab,
        })
    }
}

/// Creates `dir` (mode 0700) if absent and refuses one that group or others can reach.
/// Checks the secrets directory, creating it (mode 0700) only with `create`: a
/// hub without GitHub App connections never needs it, e.g. on a read-only root.
pub fn prepare_secrets_dir(dir: &FsPath, create: bool) -> Result<()> {
    if !create && !dir.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        if !dir.exists() {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
        }
        if !dir.is_dir() || dir.metadata()?.permissions().mode() & 0o077 != 0 {
            return Err(HubError::Invalid(
                "--secrets must be a directory not accessible by group or others".into(),
            ));
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// What `<secrets>/<connection>.json` holds. Never logged or sent.
#[derive(Deserialize, Serialize)]
struct AppFile {
    app_id: i64,
    slug: String,
    client_id: String,
    client_secret: String,
    webhook_secret: String,
    pem: String,
}

struct AppCred {
    app_id: i64,
    key: AppKey,
    webhook_secret: String,
}

struct Setup {
    connection: String,
    name: String,
    org: Option<String>,
    admin: String,
}

pub struct Sync {
    config: Option<SyncConfig>,
    api: Option<Api>,
    gitlab: Option<gitlab::Api>,
    apps: Mutex<HashMap<String, Arc<AppCred>>>,
    /// Installation id to (token, unix expiry).
    installs: Mutex<HashMap<i64, (String, i64)>>,
    setups: Mutex<Table<Setup>>,
    jobs: Semaphore,
    repo_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Caps that cut the last poll's view of a repository short.
    truncated: Mutex<HashMap<String, Value>>,
    pending: AtomicUsize,
    /// Rotates which repository a connection's poll starts with.
    round: AtomicUsize,
    /// Per repository id, the last pull request number the forge check pass finished with.
    #[cfg(unix)]
    forge_cursors: Mutex<HashMap<String, i64>>,
    /// Per repository id, where the external run pass left off: the last pull request number
    /// checked for a merge and the last merged publication id read.
    external_cursors: Mutex<HashMap<String, (i64, String)>>,
    /// Files already noticed per (intent, repository, pull request number).
    noticed: Mutex<HashMap<(String, String, i64), BTreeSet<String>>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

impl Sync {
    #[cfg(unix)]
    pub(super) fn forge_cursor(&self, repository: &str) -> i64 {
        lock(&self.forge_cursors)
            .get(repository)
            .copied()
            .unwrap_or(0)
    }

    #[cfg(unix)]
    pub(super) fn set_forge_cursor(&self, repository: &str, number: i64) {
        lock(&self.forge_cursors).insert(repository.to_owned(), number);
    }

    /// Whether external runs are read for this GitHub repository (never, unless listed).
    pub(super) fn external_runs_enabled(&self, full_name: &str) -> bool {
        self.config
            .as_ref()
            .is_some_and(|c| c.external_runs.contains(&full_name.to_ascii_lowercase()))
    }

    pub(super) fn external_cursor(&self, repository: &str) -> (i64, String) {
        lock(&self.external_cursors)
            .get(repository)
            .cloned()
            .unwrap_or_default()
    }

    pub(super) fn set_external_cursor(&self, repository: &str, cursor: (i64, String)) {
        lock(&self.external_cursors).insert(repository.to_owned(), cursor);
    }

    pub fn unconfigured() -> Self {
        Self::build(None, None, None)
    }

    pub fn new(config: SyncConfig) -> Result<Self> {
        let api = Api::new(&config.api, &config.web)?;
        let gitlab = config
            .gitlab
            .as_ref()
            .map(|gitlab| gitlab::Api::new(&gitlab.api))
            .transpose()?;
        Ok(Self::build(Some(config), Some(api), gitlab))
    }

    fn build(config: Option<SyncConfig>, api: Option<Api>, gitlab: Option<gitlab::Api>) -> Self {
        Self {
            config,
            api,
            gitlab,
            apps: Mutex::default(),
            installs: Mutex::default(),
            setups: Mutex::default(),
            jobs: Semaphore::new(MAX_JOBS),
            repo_locks: Mutex::default(),
            truncated: Mutex::default(),
            pending: AtomicUsize::new(0),
            round: AtomicUsize::new(0),
            #[cfg(unix)]
            forge_cursors: Mutex::default(),
            external_cursors: Mutex::default(),
            noticed: Mutex::default(),
        }
    }

    fn configured(&self) -> std::result::Result<(&SyncConfig, &Api), ApiError> {
        match (&self.config, &self.api) {
            (Some(config), Some(api)) => Ok((config, api)),
            _ => Err(ApiError::new(StatusCode::NOT_FOUND, "not_configured")),
        }
    }

    pub(super) async fn lock_repo(&self, repo: &str) -> OwnedMutexGuard<()> {
        let mutex = lock(&self.repo_locks)
            .entry(repo.to_owned())
            .or_default()
            .clone();
        mutex.lock_owned().await
    }

    fn load_app(&self, connection: &str) -> Result<()> {
        let config = self
            .config
            .as_ref()
            .ok_or_else(|| HubError::Invalid("sync is not configured".into()))?;
        let text = read_private(&config.secrets.join(format!("{connection}.json")))?;
        let file: AppFile = serde_json::from_str(&text)
            .map_err(|_| HubError::Invalid("app credentials file is not valid".into()))?;
        let cred = AppCred {
            app_id: file.app_id,
            key: AppKey::from_pem(&file.pem)?,
            webhook_secret: file.webhook_secret,
        };
        lock(&self.apps).insert(connection.to_owned(), Arc::new(cred));
        Ok(())
    }

    fn webhook_secret(&self, conn: &Connection) -> Option<String> {
        if conn.kind == "pat" {
            return self.pat(conn)?.webhook_secret.clone();
        }
        // An app made without a webhook has no secret; an empty key would accept forgeries.
        Some(lock(&self.apps).get(&conn.id)?.webhook_secret.clone()).filter(|s| !s.is_empty())
    }

    fn app(&self, conn: &Connection) -> Gh<Arc<AppCred>> {
        lock(&self.apps)
            .get(&conn.id)
            .cloned()
            .ok_or_else(|| GhError::Failed("no app credentials loaded for the connection".into()))
    }

    async fn jwt(&self, conn: &Connection) -> Gh<String> {
        let app = self.app(conn)?;
        Ok(app.key.jwt(app.app_id, unix_now())?)
    }

    /// A bearer token for reading `repo` (or for app-level calls when `None`).
    pub(super) async fn token(&self, conn: &Connection, installation: Option<i64>) -> Gh<String> {
        if conn.kind == "pat" {
            return self
                .pat(conn)
                .map(|pat| pat.token.clone())
                .ok_or_else(|| GhError::Failed("no PAT configured".into()));
        }
        let installation =
            installation.ok_or_else(|| GhError::Failed("repository has no installation".into()))?;
        if let Some((token, expires)) = lock(&self.installs).get(&installation)
            && *expires > unix_now() + TOKEN_SLACK_SECONDS
        {
            return Ok(token.clone());
        }
        let api = self
            .api
            .as_ref()
            .ok_or_else(|| GhError::Failed("sync is not configured".into()))?;
        let jwt = self.jwt(conn).await?;
        let (token, expires) = api.installation_token(&jwt, installation).await?;
        lock(&self.installs).insert(installation, (token.clone(), expires));
        Ok(token)
    }

    /// The configured token of a `pat` connection's provider.
    fn pat(&self, conn: &Connection) -> Option<&Pat> {
        let config = self.config.as_ref()?;
        match conn.provider.as_str() {
            "gitlab" => config.gitlab.as_ref().map(|gitlab| &gitlab.token),
            _ => config.pat.as_ref(),
        }
    }

    /// The client for the connection's provider.
    pub(super) fn forge(&self, conn: &Connection) -> Gh<ForgeApi<'_>> {
        let api = match conn.provider.as_str() {
            "gitlab" => self.gitlab.as_ref().map(ForgeApi::Gitlab),
            _ => self.api.as_ref().map(ForgeApi::Github),
        };
        api.ok_or_else(|| GhError::Failed("sync is not configured".into()))
    }

    /// The identity hosts of the configured forges.
    pub(super) fn hosts(&self) -> Vec<String> {
        self.config
            .iter()
            .flat_map(|config| {
                std::iter::once(config.host.clone())
                    .chain(config.gitlab.iter().map(|g| g.host.clone()))
            })
            .collect()
    }

    pub(super) fn api(&self) -> Gh<&Api> {
        self.api
            .as_ref()
            .ok_or_else(|| GhError::Failed("sync is not configured".into()))
    }

    pub(super) fn config(&self) -> Gh<&SyncConfig> {
        self.config
            .as_ref()
            .ok_or_else(|| GhError::Failed("sync is not configured".into()))
    }
}

/// A private regular file's text.
fn read_private(path: &FsPath) -> Result<String> {
    use std::io::Read;
    let bad = |why: &str| HubError::Invalid(format!("credentials file {why}"));
    let meta = std::fs::metadata(path).map_err(|_| bad("cannot be read"))?;
    if !meta.is_file() {
        return Err(bad("must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(bad("must not be accessible by group or others"));
        }
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_CREDENTIAL_FILE_BYTES + 1)
                .read_to_string(&mut text)
        })
        .map_err(|_| bad("cannot be read"))?;
    if text.len() as u64 > MAX_CREDENTIAL_FILE_BYTES {
        return Err(bad("is too large"));
    }
    Ok(text)
}

/// Writes `bytes` to `path` with mode 0600: a private temp file renamed into place.
fn write_private(path: &FsPath, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".tmp-{}", std::process::id()));
    let temp = PathBuf::from(temp);
    let _ = std::fs::remove_file(&temp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&temp)?.write_all(bytes)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

async fn db<T: Send + 'static>(
    state: &HubState,
    work: impl FnOnce(&dyn Store) -> Result<T> + Send + 'static,
) -> Result<T> {
    let store = state.store.clone();
    blocking(state, move || work(&*store)).await
}

fn gh_to_hub(error: GhError) -> HubError {
    forge_to_hub("GitHub", error)
}

fn forge_to_hub(who: &str, error: GhError) -> HubError {
    match error {
        GhError::Auth(detail) | GhError::Failed(detail) => {
            HubError::Unavailable(format!("{who}: {detail}"))
        }
        GhError::Limited(_) => HubError::Unavailable(format!("{who}: rate limited")),
    }
}

/// The provider's name for operator-facing text.
pub(super) fn who(conn: &Connection) -> &'static str {
    if conn.provider == "gitlab" {
        "GitLab"
    } else {
        "GitHub"
    }
}

// ---- startup ----

/// Loads app credentials for the stored connections and makes sure the PAT, if
/// one is configured, has its connection.
pub async fn start(state: &HubState) -> Result<()> {
    let sync = &state.sync;
    let Some(config) = &sync.config else {
        return Ok(());
    };
    for conn in db(state, |store| store.connections()).await? {
        if conn.kind == "app"
            && let Err(error) = sync.load_app(&conn.id)
        {
            eprintln!(
                "hub: credentials of GitHub connection {} unavailable: {error}",
                conn.id
            );
        }
    }
    if let Some(pat) = &config.pat {
        let user = sync.api().map_err(gh_to_hub)?.user(&pat.token).await;
        ensure_pat(state, "github", &config.host, "GitHub", "PAT", user).await?;
    }
    if let (Some(gitlab), Some(api)) = (&config.gitlab, &sync.gitlab) {
        let user = api.user(&gitlab.token.token).await;
        ensure_pat(state, "gitlab", &gitlab.host, "GitLab", "token", user).await?;
    }
    Ok(())
}

/// Makes sure the provider's token has its `pat` connection, labelled for the account it acts as.
async fn ensure_pat(
    state: &HubState,
    provider: &str,
    host: &str,
    who: &str,
    what: &str,
    user: Gh<(i64, String)>,
) -> Result<()> {
    let login = match user {
        Ok((_, login)) => login,
        Err(GhError::Auth(detail)) => {
            return Err(HubError::Invalid(format!(
                "{who} refused the {what} ({detail})"
            )));
        }
        // A blip at startup must not keep the hub down; a restart will create the connection.
        Err(error) => {
            eprintln!(
                "hub: {who} {what} not checked: {}",
                forge_to_hub(who, error)
            );
            return Ok(());
        }
    };
    let host = host.to_owned();
    let existing = db(state, {
        let host = host.clone();
        move |store| store.pat_connection(&host)
    })
    .await?;
    match existing {
        Some(conn)
            if !conn
                .acts_as
                .as_deref()
                .is_some_and(|recorded| recorded.eq_ignore_ascii_case(&login)) =>
        {
            return Err(HubError::Invalid(format!(
                "the {who} {what} now acts as {login}, but connection {} is labelled for another \
                 account; its label would be wrong. Use the original token",
                conn.id
            )));
        }
        Some(_) => {}
        None => {
            let new = NewConnection {
                id: new_connection_id()?,
                provider: provider.into(),
                host,
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some(login),
            };
            db(state, move |store| store.insert_connection(&new, "system")).await?;
        }
    }
    Ok(())
}

/// Polls forever, one round every `seconds`; errors go to stderr.
pub async fn poll_loop(state: HubState, seconds: u64) {
    loop {
        if let Err(error) = poll_once(&state).await {
            eprintln!("hub: poll failed: {error}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(seconds)).await;
    }
}

// ---- polling ----

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PollReport {
    pub connection: String,
    pub repository: String,
    /// Open pull requests read.
    pub pull_requests: usize,
    /// Observations that changed.
    pub changed: usize,
    pub branches: usize,
    /// The open pull request list was unchanged (304); nothing was written for it.
    pub not_modified: bool,
    /// Null, or caps that cut the view short: `{"pull_requests": <pages unread>}`.
    pub truncated: Value,
}

impl PollReport {
    pub fn to_json(&self) -> Value {
        json!({"connection": self.connection, "repository": self.repository,
            "pull_requests": self.pull_requests, "changed": self.changed,
            "branches": self.branches, "not_modified": self.not_modified,
            "truncated": self.truncated})
    }
}

/// One round: every connection whose `next_poll_at` has passed.
pub async fn poll_once(state: &HubState) -> Result<Vec<PollReport>> {
    poll(state, None).await
}

/// Discovery and a poll of one connection now, ignoring its `next_poll_at`.
pub async fn refresh(state: &HubState, connection: &str) -> Result<Vec<PollReport>> {
    poll(state, Some(connection)).await
}

async fn poll(state: &HubState, only: Option<&str>) -> Result<Vec<PollReport>> {
    let mut reports = Vec::new();
    if state.sync.config.is_none() {
        return Ok(reports);
    }
    for conn in db(state, |store| store.connections()).await? {
        let due = match only {
            Some(id) => conn.id == id,
            None => conn.next_poll_at <= unix_now(),
        };
        if due {
            reports.extend(poll_connection(state, &conn).await?);
        }
    }
    let before = now_ms() - TOMBSTONE_KEEP_MS;
    db(state, move |store| store.prune_tombstones(before)).await?;
    Ok(reports)
}

/// What to do after a failed call. Returns true when the connection should stop for now.
pub(super) async fn failed(
    state: &HubState,
    conn: &Connection,
    what: &str,
    error: GhError,
) -> Result<bool> {
    match error {
        GhError::Auth(detail) => {
            lock(&state.sync.installs).clear();
            let (id, why) = (conn.id.clone(), detail);
            db(state, move |store| {
                store.set_connection_status(&id, "auth_failed", Some(&why))
            })
            .await?;
            Ok(true)
        }
        GhError::Limited(until) => {
            let id = conn.id.clone();
            db(state, move |store| store.set_next_poll(&id, until)).await?;
            Ok(true)
        }
        GhError::Failed(detail) => {
            eprintln!("hub: {} sync of {what}: {detail}", conn.provider);
            Ok(false)
        }
    }
}

async fn mark_enabled(state: &HubState, conn: &Connection) -> Result<()> {
    if conn.status != "enabled" {
        let id = conn.id.clone();
        db(state, move |store| {
            store.set_connection_status(&id, "enabled", None)
        })
        .await?;
    }
    Ok(())
}

async fn poll_connection(state: &HubState, conn: &Connection) -> Result<Vec<PollReport>> {
    let mut reports = Vec::new();
    let mut marked = false;
    if conn.kind == "app" {
        match discover(state, conn).await {
            Ok(()) => {
                mark_enabled(state, conn).await?;
                marked = true;
            }
            Err(error) => {
                failed(state, conn, "installations", error).await?;
                return Ok(reports);
            }
        }
    }
    let id = conn.id.clone();
    let mut repos = db(state, move |store| store.repositories(&id)).await?;
    // Start somewhere else each round, so a limit or a slow repository does not starve the rest.
    if !repos.is_empty() {
        let start = state.sync.round.fetch_add(1, Ordering::Relaxed) % repos.len();
        repos.rotate_left(start);
    }
    for repo in repos {
        match sync_repo(state, conn, &repo).await {
            Ok(report) => {
                if !marked {
                    mark_enabled(state, conn).await?;
                    marked = true;
                }
                reports.push(report);
            }
            Err(error) => {
                if failed(state, conn, &repo.full_name, error).await? {
                    break;
                }
            }
        }
    }
    Ok(reports)
}

/// Connects every repository the app's installations can see.
async fn discover(state: &HubState, conn: &Connection) -> Gh<()> {
    let sync = &state.sync;
    let api = sync.api()?;
    let jwt = sync.jwt(conn).await?;
    for (installation, _) in api.installations(&jwt).await? {
        let token = sync.token(conn, Some(installation)).await?;
        let repos = api.installation_repos(&token).await?;
        let (id, host) = (conn.id.clone(), conn.host.clone());
        db(state, move |store| {
            for repo in repos {
                store.upsert_repository(
                    &id,
                    &host,
                    repo.repository_id,
                    &repo.full_name,
                    repo.node_id.as_deref(),
                    Some(installation),
                )?;
            }
            Ok(())
        })
        .await?;
    }
    Ok(())
}

/// Stored pull request observations by number.
async fn stored_pulls(state: &HubState, repo: &ConnectedRepository) -> Gh<BTreeMap<i64, Value>> {
    let id = repo.id.clone();
    let rows = db(state, move |store| store.observations(&id, "pull_request")).await?;
    Ok(rows
        .iter()
        .filter_map(|row| Some((row.key.parse().ok()?, serde_json::from_str(&row.data).ok()?)))
        .collect())
}

/// A repository-level 401 or 403 says nothing about the connection's credential, so for an
/// app the repository is skipped this round and its installation token is dropped.
fn repo_level(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    error: GhError,
) -> GhError {
    match error {
        GhError::Auth(detail) if conn.kind == "app" => {
            if let Some(installation) = repo.installation_id {
                lock(&state.sync.installs).remove(&installation);
            }
            GhError::Failed(format!("{detail} on the repository"))
        }
        other => other,
    }
}

async fn sync_repo(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
) -> Gh<PollReport> {
    let sync = &state.sync;
    let _permit = sync
        .jobs
        .acquire()
        .await
        .map_err(|_| GhError::Failed("shutting down".into()))?;
    let _guard = sync.lock_repo(&repo.id).await;
    // A failure to get a token is the connection's credential failing.
    let token = sync.token(conn, repo.installation_id).await?;
    read_repo(state, conn, repo, &token)
        .await
        .map_err(|error| repo_level(state, conn, repo, error))
}

async fn read_repo(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
) -> Gh<PollReport> {
    let sync = &state.sync;
    let (api, config) = (sync.forge(conn)?, sync.config()?);
    let mut report = PollReport {
        connection: conn.id.clone(),
        repository: repo.full_name.clone(),
        truncated: Value::Null,
        ..PollReport::default()
    };
    let mut cut = serde_json::Map::new();

    // Pull requests.
    let started = now_ms();
    let stored = stored_pulls(state, repo).await?;
    match api
        .open_pulls(token, repo, repo.pulls_etag.as_deref())
        .await?
    {
        None => {
            report.not_modified = true;
            // The pull side is as the last poll left it; branches are recomputed below.
            if let Some(Value::Object(old)) = lock(&sync.truncated).get(&repo.id) {
                cut.extend(
                    old.iter()
                        .filter(|(key, _)| *key != "branches")
                        .map(|(k, v)| (k.clone(), v.clone())),
                );
            }
        }
        Some(paged) => {
            let mut seen = BTreeSet::new();
            for item in &paged.items {
                let Some(number) = item["number"].as_i64() else {
                    continue;
                };
                seen.insert(number);
                if observe_pull(state, conn, repo, token, item, stored.get(&number), started)
                    .await?
                {
                    report.changed += 1;
                }
            }
            report.pull_requests = paged.items.len();
            let gone: Vec<i64> = stored
                .keys()
                .filter(|n| !seen.contains(n))
                .copied()
                .collect();
            let deferred = gone.len().saturating_sub(MAX_REREADS);
            for number in gone.into_iter().take(MAX_REREADS) {
                if reread_pull(state, conn, repo, token, number, &stored).await? {
                    report.changed += 1;
                }
            }
            if paged.unread > 0 {
                cut.insert("pull_requests".into(), json!(paged.unread));
            }
            if deferred > 0 {
                cut.insert("rereads_deferred".into(), json!(deferred));
            }
            // Page 1's ETag says nothing about later pages, so only a complete single page is cached.
            let etag = paged
                .etag
                .filter(|_| paged.pages == 1 && paged.unread == 0 && deferred == 0);
            let id = repo.id.clone();
            db(state, move |store| {
                store.set_pulls_etag(&id, etag.as_deref())
            })
            .await?;
        }
    }

    // Branches.
    let (count, capped) = sync_branches(state, conn, repo, token, config.activity_days).await?;
    report.branches = count;
    if capped {
        cut.insert("branches".into(), json!(MAX_RECENT_BRANCHES));
    }
    report.truncated = if cut.is_empty() {
        Value::Null
    } else {
        Value::Object(cut)
    };
    lock(&sync.truncated).insert(repo.id.clone(), report.truncated.clone());

    if conn.outbound {
        outbound_repo(state, conn, repo, token).await?;
    }
    #[cfg(unix)]
    super::forge_check::reconcile_repo(state, conn, repo, token, None).await?;
    super::external_runs::reconcile_repo(state, conn, repo, token).await?;
    Ok(report)
}

/// Observes a pull request from its API JSON; true when the observation changed.
async fn observe_pull(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    item: &Value,
    stored: Option<&Value>,
    started: i64,
) -> Gh<bool> {
    let Some(number) = item["number"].as_i64() else {
        return Ok(false);
    };
    let head_sha = item["head"]["sha"].as_str().unwrap_or_default();
    // Files are only read again when the head moved.
    let (files, truncated) = match stored {
        // On GitLab, empty and not truncated may be a diff not generated yet: read it again.
        Some(old)
            if old["head_sha"] == head_sha
                && (conn.provider != "gitlab"
                    || old["files_truncated"].as_bool().unwrap_or(false)
                    || old["files"].as_array().is_some_and(|f| !f.is_empty())) =>
        {
            (
                old["files"].clone(),
                old["files_truncated"].as_bool().unwrap_or(false),
            )
        }
        _ => {
            let (files, truncated) = state
                .sync
                .forge(conn)?
                .pull_files(token, repo, number)
                .await?;
            (json!(files), truncated)
        }
    };
    let data = json!({"number": number, "node_id": item["node_id"], "title": item["title"],
        "state": item["state"], "draft": item["draft"].as_bool().unwrap_or(false),
        "head_ref": item["head"]["ref"], "head_sha": head_sha, "base_ref": item["base"]["ref"],
        "author": {"login": item["user"]["login"], "id": item["user"]["id"]},
        "html_url": item["html_url"],
        "files": files, "files_truncated": truncated});
    let (id, text) = (repo.id.clone(), data.to_string());
    let observed = db(state, move |store| {
        store.observe(&id, "pull_request", &number.to_string(), &text, started)
    })
    .await?;
    let Observed::Changed { .. } = observed else {
        return Ok(false);
    };
    notify_overlaps(state, conn, repo, &data).await?;
    Ok(true)
}

/// Re-reads one pull request: observed while open, forgotten once closed or gone.
async fn reread_pull(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    number: i64,
    stored: &BTreeMap<i64, Value>,
) -> Gh<bool> {
    let started = now_ms();
    let pull = state.sync.forge(conn)?.pull(token, repo, number).await?;
    if let Some(pull) = pull.as_ref().filter(|pull| pull["state"] == "open") {
        return observe_pull(state, conn, repo, token, pull, stored.get(&number), started).await;
    }
    let data = match &pull {
        None => json!({"number": number, "state": "gone", "merged": false}),
        Some(pull) => json!({"number": number, "node_id": pull["node_id"],
            "state": pull["state"], "merged": pull["merged"].as_bool().unwrap_or(false),
            "merge_commit_sha": pull["merge_commit_sha"], "head_sha": pull["head"]["sha"],
            "head_ref": pull["head"]["ref"], "html_url": pull["html_url"]}),
    }
    .to_string();
    let (id, key) = (repo.id.clone(), number.to_string());
    let forgotten = db(state, move |store| {
        store.forget(
            &id,
            "pull_request",
            &key,
            started,
            &data,
            "pull_request_closed",
        )
    })
    .await?;
    if forgotten {
        lock(&state.sync.noticed).retain(|k, _| !(k.1 == repo.id && k.2 == number));
        if let (Some(pull), Some(old)) = (&pull, stored.get(&number))
            && pull["merged"] == true
        {
            notify_merged(state, conn, repo, pull, old).await?;
        }
    }
    Ok(forgotten)
}

/// Re-reads one branch: `Some(changed)` while it exists, `None` once it is gone.
async fn reread_branch(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    name: &str,
) -> Gh<Option<bool>> {
    let started = now_ms();
    let branch = state.sync.forge(conn)?.branch(token, repo, name).await?;
    let (id, name) = (repo.id.clone(), name.to_owned());
    Ok(db(state, move |store| match branch {
        Some(branch) => {
            let observed = store.observe(&id, "branch", &name, &branch.data(), started)?;
            Ok(Some(matches!(observed, Observed::Changed { .. })))
        }
        None => {
            let data = json!({"name": name, "deleted": true}).to_string();
            store.forget(&id, "branch", &name, started, &data, "branch_deleted")?;
            Ok(None)
        }
    })
    .await?)
}

/// Recent branches plus those live intents name. A tracked branch that is neither is
/// dropped (`branch_unobserved`); a named one that is gone is `branch_deleted`.
/// Returns the branches read and whether the recent-branch cap cut the list short.
async fn sync_branches(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    activity_days: i64,
) -> Gh<(usize, bool)> {
    let started = now_ms();
    let (recent, capped) = state
        .sync
        .forge(conn)?
        .recent_branches(token, repo, unix_now() - activity_days * 86_400)
        .await?;
    let recent_names: BTreeSet<String> = recent.iter().map(|b| b.name.clone()).collect();
    let named: BTreeSet<String> = repo_intents(state, conn, repo)
        .await?
        .into_iter()
        .filter_map(|intent| intent.branch)
        .filter(|name| valid_ref_name(name))
        .collect();
    let id = repo.id.clone();
    let tracked = db(state, move |store| store.observations(&id, "branch")).await?;
    let stale: Vec<String> = tracked
        .into_iter()
        .map(|row| row.key)
        .filter(|name| !recent_names.contains(name) && !named.contains(name))
        .collect();
    let lookups: Vec<String> = named
        .into_iter()
        .filter(|name| !recent_names.contains(name))
        .take(MAX_NAMED_BRANCHES)
        .collect();
    let mut found = recent.len();
    let (id, batch): (String, Vec<Branch>) = (repo.id.clone(), recent);
    db(state, move |store| {
        for branch in &batch {
            store.observe(&id, "branch", &branch.name, &branch.data(), started)?;
        }
        for name in &stale {
            let data = json!({"name": name}).to_string();
            store.forget(&id, "branch", name, started, &data, "branch_unobserved")?;
        }
        Ok(())
    })
    .await?;
    for name in lookups {
        if reread_branch(state, conn, repo, token, &name)
            .await?
            .is_some()
        {
            found += 1;
        }
    }
    Ok((found, capped))
}

// ---- overlap with intents ----

/// Live intents in the repository whose owners are on the connection's host.
async fn repo_intents(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
) -> Gh<Vec<Intent>> {
    let name = repo.full_name.clone();
    let intents = db(state, move |store| {
        store.live_intents(&name, unix_now(), MAX_LIVE_INTENTS)
    })
    .await?;
    Ok(intents
        .into_iter()
        .filter(|i| {
            i.owner.identity.host == conn.host && i.owner.identity.provider == conn.provider
        })
        .collect())
}

/// (files that overlap any spec, specs that overlap any file).
fn pull_overlap(files: &[String], specs: &[String]) -> (Vec<String>, Vec<String>) {
    let theirs = files
        .iter()
        .filter(|file| specs.iter().any(|spec| overlaps(file, key_of(spec))))
        .cloned()
        .collect();
    let yours = specs
        .iter()
        .filter(|spec| files.iter().any(|file| overlaps(file, key_of(spec))))
        .cloned()
        .collect();
    (theirs, yours)
}

fn files_of(data: &Value) -> Vec<String> {
    data["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|f| f.as_str())
        // A name with control characters could break out of a comment or notice line.
        .filter(|f| !f.chars().any(char::is_control))
        .map(str::to_owned)
        .collect()
}

/// One `pr_notice` per live intent for files it overlaps that it was not told about before.
async fn notify_overlaps(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    data: &Value,
) -> Gh<()> {
    let files = files_of(data);
    let number = data["number"].as_i64().unwrap_or_default();
    let notices: Vec<(String, String)> = repo_intents(state, conn, repo)
        .await?
        .iter()
        .filter_map(|intent| {
            let (theirs, yours) = pull_overlap(&files, &intent.paths);
            if theirs.is_empty() {
                return None;
            }
            let key = (intent.id.clone(), repo.id.clone(), number);
            let mut noticed = lock(&state.sync.noticed);
            // shortcut: forget everything when full, so at worst some notices repeat once
            if noticed.len() >= MAX_NOTICED && !noticed.contains_key(&key) {
                noticed.clear();
            }
            let told = noticed.entry(key).or_default();
            let fresh = theirs.iter().any(|file| !told.contains(file));
            told.extend(theirs.iter().cloned());
            fresh.then(|| {
                let notice = pr_notice(repo, data, &theirs, &yours);
                (intent.id.clone(), notice.to_string())
            })
        })
        .collect();
    append_pr_notices(state, notices).await
}

/// One `pr_notice` with `merged: true` per live intent overlapping a just-merged PR's files, so
/// agents still editing them sync with the base before going on. Not the PR's own branch.
async fn notify_merged(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    pull: &Value,
    observed: &Value,
) -> Gh<()> {
    let files = files_of(observed);
    let head = observed["head_ref"].as_str();
    let notices: Vec<(String, String)> = repo_intents(state, conn, repo)
        .await?
        .iter()
        .filter(|intent| head.is_none() || intent.branch.as_deref() != head)
        .filter_map(|intent| {
            let (theirs, yours) = pull_overlap(&files, &intent.paths);
            (!theirs.is_empty()).then(|| {
                let mut notice = pr_notice(repo, observed, &theirs, &yours);
                notice["merged"] = json!(true);
                notice["base_ref"] = observed["base_ref"].clone();
                notice["merge_commit_sha"] = pull["merge_commit_sha"].clone();
                (intent.id.clone(), notice.to_string())
            })
        })
        .collect();
    append_pr_notices(state, notices).await
}

/// A `pr_notice` body for the observed pull request `data`.
fn pr_notice(
    repo: &ConnectedRepository,
    data: &Value,
    theirs: &[String],
    yours: &[String],
) -> Value {
    json!({"about": "pull_request", "repository": repo.full_name,
        "repository_id": repo.repository_id, "number": data["number"],
        "node_id": data["node_id"], "title": data["title"],
        "html_url": data["html_url"], "head_ref": data["head_ref"],
        "head_sha": data["head_sha"], "author": data["author"],
        "their_paths": theirs, "your_paths": yours})
}

async fn append_pr_notices(state: &HubState, notices: Vec<(String, String)>) -> Gh<()> {
    if !notices.is_empty() {
        db(state, move |store| {
            store.append_notices("pr_notice", &notices)
        })
        .await?;
        NOTICES.notify_waiters();
    }
    Ok(())
}

/// The repository a token is for: by the forge's id when the token has one, else by name.
pub(super) fn find_repository(
    store: &dyn Store,
    host: &str,
    name: &str,
    repository_id: Option<i64>,
) -> Result<Option<ConnectedRepository>> {
    match repository_id {
        Some(id) => store.repository_by_id(host, id),
        None => store.repository_named(host, name),
    }
}

/// The open observed pull requests of a repository, as stored.
async fn observed_pulls(
    state: &HubState,
    host: &str,
    repository: &str,
    repository_id: Option<i64>,
) -> Result<Vec<Value>> {
    let (host, name) = (host.to_owned(), repository.to_owned());
    let rows = db(state, move |store| {
        match find_repository(store, &host, &name, repository_id)? {
            Some(repo) => store.observations(&repo.id, "pull_request"),
            None => Ok(Vec::new()),
        }
    })
    .await?;
    Ok(rows
        .iter()
        .filter_map(|row| serde_json::from_str::<Value>(&row.data).ok())
        .collect())
}

/// Open observed pull requests that overlap `specs`, for an announce reply.
pub(super) async fn overlapping_pulls(
    state: &HubState,
    host: &str,
    repository: &str,
    repository_id: Option<i64>,
    specs: &[String],
) -> Result<Vec<Value>> {
    let pulls = observed_pulls(state, host, repository, repository_id).await?;
    Ok(pulls
        .iter()
        .filter_map(|pull| {
            let (theirs, yours) = pull_overlap(&files_of(pull), specs);
            (!theirs.is_empty()).then(|| {
                json!({"number": pull["number"], "title": pull["title"],
                    "html_url": pull["html_url"], "author": pull["author"],
                    "their_paths": theirs, "your_paths": yours})
            })
        })
        .collect())
}

/// Every open observed pull request with the files it changes, for a status reply.
pub(super) async fn open_pulls(
    state: &HubState,
    host: &str,
    repository: &str,
    repository_id: Option<i64>,
) -> Result<Vec<Value>> {
    let pulls = observed_pulls(state, host, repository, repository_id).await?;
    Ok(pulls
        .iter()
        .map(|pull| {
            json!({"number": pull["number"], "title": pull["title"],
                "html_url": pull["html_url"], "author": pull["author"],
                "their_paths": files_of(pull)})
        })
        .collect())
}

// ---- outbound ----

fn marker(repo: &ConnectedRepository, number: i64) -> String {
    format!("<!-- {MARKER_PREFIX}:{}:{number} -->", repo.repository_id)
}

/// Text from a user or an agent, safe inside a code span.
fn code(text: &str, max_chars: usize) -> String {
    let flat: String = text
        .chars()
        .take(max_chars)
        .map(|c| {
            if c == '`' {
                '\''
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    format!("`{flat}`")
}

fn comment_body(
    repo: &ConnectedRepository,
    pull: &Value,
    overlaps: &[(Intent, Vec<String>)],
) -> String {
    let number = pull["number"].as_i64().unwrap_or_default();
    let head = pull["head_sha"].as_str().unwrap_or_default();
    let mut body = format!(
        "{}\n**AgentSCM coordination** (head `{}`)\n\n",
        marker(repo, number),
        &head[..head.len().min(7)]
    );
    if overlaps.is_empty() {
        body.push_str("No live declared intent overlaps this pull request.\n");
        return body;
    }
    body.push_str(&format!(
        "{} declared intents overlap this pull request:\n\n",
        overlaps.len()
    ));
    for (intent, paths) in overlaps {
        let shown: Vec<String> = paths
            .iter()
            .take(COMMENT_PATHS)
            .map(|p| code(p, 200))
            .collect();
        let more = paths.len().saturating_sub(COMMENT_PATHS);
        body.push_str(&format!(
            "- {} by agent {} (owner {}): {}{}\n",
            code(&intent.task, COMMENT_TASK_CHARS),
            code(&intent.agent, 64),
            code(&intent.owner.login, 64),
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ));
    }
    body
}

/// Writes the comment and status for every observed open pull request of `repo`.
async fn outbound_repo(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
) -> Gh<()> {
    let pulls = stored_pulls(state, repo).await?;
    let intents = repo_intents(state, conn, repo).await?;
    for (number, pull) in pulls {
        let files = files_of(&pull);
        let overlapping: Vec<(Intent, Vec<String>)> = intents
            .iter()
            .filter_map(|intent| {
                let (theirs, _) = pull_overlap(&files, &intent.paths);
                (!theirs.is_empty()).then(|| (intent.clone(), theirs))
            })
            .collect();
        match outbound_pull(state, conn, repo, token, number, &pull, &overlapping).await {
            Err(GhError::Failed(detail)) => {
                eprintln!(
                    "hub: {} outbound for {}#{number}: {detail}",
                    conn.provider, repo.full_name
                );
            }
            other => other?,
        }
    }
    Ok(())
}

/// The login this connection's comments appear under.
fn own_login(conn: &Connection) -> String {
    match conn.kind.as_str() {
        "app" => format!("{}[bot]", conn.app_slug.as_deref().unwrap_or_default()),
        _ => conn.acts_as.clone().unwrap_or_default(),
    }
}

async fn outbound_pull(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    number: i64,
    pull: &Value,
    overlapping: &[(Intent, Vec<String>)],
) -> Gh<()> {
    let api = state.sync.forge(conn)?;
    let body = comment_body(repo, pull, overlapping);
    let sha = lower_hex_of(body.as_bytes());
    let (id, rep_id) = (repo.id.clone(), repo.id.clone());
    let out = db(state, move |store| store.outbound(&id, number)).await?;
    let comment = out.as_ref().and_then(|o| o.comment_id);
    let known_sha = out.as_ref().and_then(|o| o.body_sha256.clone());
    let record = |comment: i64, sha: String| {
        let id = rep_id.clone();
        db(state, move |store| {
            store.set_outbound_comment(&id, number, comment, &sha)
        })
    };
    match comment {
        None if overlapping.is_empty() => return Ok(()),
        None => {
            // A reply lost after the create leaves a comment we never recorded. Adopt it, but only
            // our own: written by this connection's login, with the marker as its first line.
            let (mark, author) = (marker(repo, number), own_login(conn));
            let existing = api.comments(token, repo, number).await?;
            let own = existing.into_iter().find(|(_, login, text)| {
                login.eq_ignore_ascii_case(&author)
                    && text.lines().next().map(str::trim_end) == Some(mark.as_str())
            });
            let mut adopted = None;
            if let Some((id, _, text)) = own {
                let kept = text == body
                    || match api.edit_comment(token, repo, number, id, &body).await {
                        Ok(edited) => edited,
                        // Cannot edit it: write our own instead.
                        Err(GhError::Failed(_)) => false,
                        Err(error) => return Err(error),
                    };
                adopted = kept.then_some(id);
            }
            let id = match adopted {
                Some(id) => id,
                None => api.create_comment(token, repo, number, &body).await?,
            };
            record(id, sha.clone()).await?;
        }
        Some(id) if known_sha.as_deref() != Some(sha.as_str()) => {
            if api.edit_comment(token, repo, number, id, &body).await? {
                record(id, sha.clone()).await?;
            } else {
                let id = api.create_comment(token, repo, number, &body).await?;
                record(id, sha.clone()).await?;
            }
        }
        Some(_) => {}
    }
    let head = pull["head_sha"].as_str().unwrap_or_default().to_owned();
    let description = format!("{} overlapping declared intents", overlapping.len());
    let current = out.as_ref().is_some_and(|o| {
        o.status_sha.as_deref() == Some(head.as_str())
            && o.status_description.as_deref() == Some(description.as_str())
    });
    if !current && !head.is_empty() {
        api.create_status(token, repo, &head, &description).await?;
        db(state, move |store| {
            store.set_outbound_status(&rep_id, number, &head, &description)
        })
        .await?;
    }
    Ok(())
}

// ---- routes ----

pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/webhooks/github/{connection}",
            post(webhook).layer(DefaultBodyLimit::max(WEBHOOK_BODY_BYTES)),
        )
        .route(
            "/v1/hub/webhooks/gitlab/{connection}",
            post(gitlab_webhook).layer(DefaultBodyLimit::max(WEBHOOK_BODY_BYTES)),
        )
        .route("/v1/hub/github/apps", post(create_app))
        .route("/v1/hub/github/setup/callback", get(setup_callback))
        .route("/v1/hub/github/setup/{state}", get(setup_page))
        .route("/v1/hub/connections", get(list_connections))
        .route("/v1/hub/connections/{id}/refresh", post(refresh_connection))
        .route("/v1/hub/connections/{id}/outbound", post(set_outbound))
        .route(
            "/v1/hub/connections/{id}/repositories",
            post(add_repository),
        )
        .route("/v1/hub/repos/{repo}/work", get(work))
}

type ApiResult<T> = std::result::Result<T, ApiError>;

fn forbidden() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "forbidden")
}

async fn webhook(
    State(state): State<HubState>,
    Path(connection): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    state.sync.configured()?;
    if !is_connection_id(&connection) {
        return Err(ApiError::not_found());
    }
    let conn = db(&state, move |store| store.connection(&connection))
        .await?
        .filter(|conn| conn.provider == "github")
        .ok_or_else(ApiError::not_found)?;
    let secret = state
        .sync
        .webhook_secret(&conn)
        .ok_or_else(ApiError::unauthorized)?;
    let signature = header_str(&headers, "x-hub-signature-256").unwrap_or_default();
    if !webhook_signature_ok(&secret, &body, signature) {
        return Err(ApiError::unauthorized());
    }
    let token_ok = |value: &str| {
        !value.is_empty()
            && value.len() <= 100
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    };
    let (Some(delivery), Some(event)) = (
        header_str(&headers, "x-github-delivery").filter(|v| token_ok(v)),
        header_str(&headers, "x-github-event").filter(|v| token_ok(v)),
    ) else {
        return Err(ApiError::invalid());
    };
    let payload: Value = serde_json::from_slice(&body).map_err(|_| ApiError::invalid())?;
    accept_delivery(state, conn, delivery.to_owned(), event.to_owned(), payload).await
}

/// GitLab's delivery as the GitHub-shaped event the job reads: ids only, state is read again.
async fn gitlab_webhook(
    State(state): State<HubState>,
    Path(connection): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Response> {
    state.sync.configured()?;
    if !is_connection_id(&connection) {
        return Err(ApiError::not_found());
    }
    let conn = db(&state, move |store| store.connection(&connection))
        .await?
        .filter(|conn| conn.provider == "gitlab")
        .ok_or_else(ApiError::not_found)?;
    let secret = state
        .sync
        .webhook_secret(&conn)
        .ok_or_else(ApiError::unauthorized)?;
    let given = header_str(&headers, "x-gitlab-token").unwrap_or_default();
    // Keyed MACs make both sides the same length, so neither the secret's length nor a
    // mismatch position leaks.
    let key = secret.as_bytes();
    if secret.is_empty()
        || given.is_empty()
        || !constant_time_eq(&hmac_sha256(key, given.as_bytes()), &hmac_sha256(key, key))
    {
        return Err(ApiError::unauthorized());
    }
    let event = header_str(&headers, "x-gitlab-event").unwrap_or_default();
    // Other event types are acknowledged before they take a delivery record or a job slot.
    if !matches!(event, "Merge Request Hook" | "Push Hook") {
        return Ok((StatusCode::ACCEPTED, Json(json!({"ignored": true}))).into_response());
    }
    let delivery = header_str(&headers, "x-gitlab-event-uuid")
        .filter(|v| !v.is_empty() && v.len() <= 100)
        .ok_or_else(ApiError::invalid)?;
    let payload: Value = serde_json::from_slice(&body).map_err(|_| ApiError::invalid())?;
    let (event, payload) = match event {
        "Merge Request Hook" => (
            "pull_request",
            json!({"repository": {"id": payload["project"]["id"]},
                "pull_request": {"number": payload["object_attributes"]["iid"]}}),
        ),
        "Push Hook" => (
            "push",
            json!({"repository": {"id": payload["project_id"]}, "ref": payload["ref"]}),
        ),
        _ => unreachable!("other events returned above"),
    };
    accept_delivery(state, conn, delivery.to_owned(), event.to_owned(), payload).await
}

/// Records a delivery once and queues its job; past the backlog cap the poller catches up.
async fn accept_delivery(
    state: HubState,
    conn: Connection,
    delivery: String,
    event: String,
    payload: Value,
) -> ApiResult<Response> {
    let id = conn.id.clone();
    let fresh = db(&state, move |store| {
        store.record_delivery(&id, &delivery, unix_now())
    })
    .await?;
    if !fresh {
        return Ok(Json(json!({"duplicate": true})).into_response());
    }
    // Past the cap the delivery stays recorded and the next poll catches up.
    if state.sync.pending.fetch_add(1, Ordering::Relaxed) >= MAX_PENDING_WEBHOOKS {
        state.sync.pending.fetch_sub(1, Ordering::Relaxed);
        eprintln!(
            "hub: {} webhook backlog full; leaving {event} to the poller",
            conn.provider
        );
    } else {
        let guard = PendingJob(state.sync.clone());
        tokio::spawn(async move {
            let _guard = guard;
            webhook_job(state, conn, event, payload).await;
        });
    }
    Ok((StatusCode::ACCEPTED, Json(json!({"accepted": true}))).into_response())
}

fn is_connection_id(id: &str) -> bool {
    id.strip_prefix("con_").is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Counts a queued or running webhook job until dropped.
struct PendingJob(Arc<Sync>);

impl Drop for PendingJob {
    fn drop(&mut self) {
        self.0.pending.fetch_sub(1, Ordering::Relaxed);
    }
}

async fn webhook_job(state: HubState, conn: Connection, event: String, payload: Value) {
    if let Err(error) = webhook_work(&state, &conn, &event, &payload).await {
        eprintln!("hub: {} webhook {event}: {error}", conn.provider);
    }
}

/// The payload is trusted only for ids and names; state is read again from GitHub.
async fn webhook_work(
    state: &HubState,
    conn: &Connection,
    event: &str,
    payload: &Value,
) -> Result<()> {
    let sync = &state.sync;
    // Backing off or marked failed: no GitHub calls from here; the poller owns recovery.
    if conn.next_poll_at > unix_now() {
        return Ok(());
    }
    let _permit = sync
        .jobs
        .acquire()
        .await
        .map_err(|_| HubError::Unavailable("shutting down".into()))?;
    if matches!(event, "installation" | "installation_repositories") {
        if let Err(error) = discover(state, conn).await {
            failed(state, conn, "installations", error).await?;
        }
        return Ok(());
    }
    if !matches!(event, "pull_request" | "push") {
        return Ok(());
    }
    let Some(repository_id) = payload["repository"]["id"].as_i64() else {
        return Ok(());
    };
    let host = conn.host.clone();
    let repo = db(state, move |store| {
        store.repository_by_id(&host, repository_id)
    })
    .await?;
    let Some(repo) = repo.filter(|repo| repo.connection == conn.id) else {
        return Ok(());
    };
    let _guard = sync.lock_repo(&repo.id).await;
    let result = async {
        let token = sync.token(conn, repo.installation_id).await?;
        let reads = async {
            match event {
                "pull_request" => {
                    let number = payload["pull_request"]["number"]
                        .as_i64()
                        .filter(|n| *n > 0);
                    if let Some(number) = number {
                        let stored = stored_pulls(state, &repo).await?;
                        reread_pull(state, conn, &repo, &token, number, &stored).await?;
                    }
                }
                _ => {
                    let name = payload["ref"]
                        .as_str()
                        .and_then(|r| r.strip_prefix("refs/heads/"));
                    if let Some(name) = name.filter(|name| valid_ref_name(name)) {
                        reread_branch(state, conn, &repo, &token, name).await?;
                    }
                }
            }
            // Only the re-read state matters to a status, never the payload. A push can move
            // the base of any pull request; a pull request event concerns only its own.
            #[cfg(unix)]
            {
                let only = (event == "pull_request")
                    .then(|| {
                        payload["pull_request"]["number"]
                            .as_i64()
                            .filter(|n| *n > 0)
                    })
                    .flatten();
                if event != "pull_request" || only.is_some() {
                    super::forge_check::reconcile_repo(state, conn, &repo, &token, only).await?;
                }
            }
            Ok(())
        };
        reads
            .await
            .map_err(|error| repo_level(state, conn, &repo, error))
    }
    .await;
    match result {
        Err(error) => failed(state, conn, &repo.full_name, error).await.map(drop),
        Ok(()) => Ok(()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewApp {
    name: String,
    org: Option<String>,
}

fn simple(text: &str, extra: &[u8], max: usize) -> bool {
    !text.is_empty()
        && text.len() <= max
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || extra.contains(&b))
}

/// Registers a single-use app setup valid for `SETUP_SECONDS`; returns its URL
/// (whose last segment is the capability) and the future connection id.
fn begin_setup(
    sync: &Sync,
    name: String,
    org: Option<String>,
    admin: String,
) -> ApiResult<(String, String)> {
    let (config, _) = sync.configured()?;
    if !simple(&name, b" -_", 34) || org.as_deref().is_some_and(|org| !simple(org, b"-", 39)) {
        return Err(ApiError::invalid());
    }
    let public = check_base_url(&config.public_url).map_err(|_| ApiError::invalid())?;
    let (secret, connection) = (random_hex()?, new_connection_id()?);
    let setup = Setup {
        connection: connection.clone(),
        name,
        org,
        admin,
    };
    lock(&sync.setups).insert(
        secret.clone(),
        unix_now() + SETUP_SECONDS,
        setup,
        unix_now(),
    );
    Ok((format!("{public}/v1/hub/github/setup/{secret}"), connection))
}

async fn create_app(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<NewApp>,
) -> ApiResult<Json<Value>> {
    let admin = human.admin()?.user.id.clone();
    let (setup_url, connection) = begin_setup(&state.sync, body.name, body.org, admin)?;
    Ok(Json(
        json!({"setup_url": setup_url, "connection": connection}),
    ))
}

/// First run: with no users, no connections, no sign-in and no PAT, nobody can start
/// a setup, so write a one-time link to `<secrets>/bootstrap-setup-url` (mode 0600).
/// Any doubt about those conditions means no link. The token stays out of the log
/// and of every error. Returns the file's path when a link was written.
pub async fn bootstrap(
    state: &HubState,
    signin: bool,
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<Option<PathBuf>> {
    // Every skip says why in one line, so a missing link is never silent.
    let skip = |why: &str| {
        eprintln!("hub: no first-run setup link: {why}");
        Ok(None)
    };
    let Some(config) = &state.sync.config else {
        return skip("GitHub sync is not configured");
    };
    let path = config.secrets.join(BOOTSTRAP_FILE);
    // A link left by an earlier run names a setup that no longer exists.
    let _ = std::fs::remove_file(&path);
    // Any sign-in lets an admin create the app the normal way; a PAT hub needs none.
    if signin {
        return skip("a sign-in provider is configured");
    }
    if config.pat.is_some() {
        return skip("a GitHub PAT is configured");
    }
    let first_run = db(state, |store| {
        Ok(store.list_users()?.is_empty() && store.connections()?.is_empty())
    })
    .await;
    match first_run {
        Ok(true) => {}
        Ok(false) => return skip("the hub already has users or connections"),
        Err(error) => return skip(&format!("could not check the store ({error})")),
    }
    let lookup = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
    let name = lookup("AGENTSCM_GITHUB_APP_NAME").unwrap_or_else(|| "agentscm-hub".into());
    let (url, _) = begin_setup(
        &state.sync,
        name,
        lookup("AGENTSCM_GITHUB_APP_ORG"),
        BOOTSTRAP_ACTOR.into(),
    )
    .map_err(|_| {
        HubError::Invalid(
            "AGENTSCM_GITHUB_APP_NAME, AGENTSCM_GITHUB_APP_ORG or the public URL is not valid"
                .into(),
        )
    })?;
    let (dir, file) = (config.secrets.clone(), path.clone());
    blocking(state, move || {
        prepare_secrets_dir(&dir, true)?;
        write_private(&file, url.as_bytes())
    })
    .await?;
    eprintln!(
        "hub: first run: open the one-time GitHub App setup link in {} within {} minutes",
        path.display(),
        SETUP_SECONDS / 60
    );
    Ok(Some(path))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn page(response: Html<String>) -> Response {
    let mut response = response.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; script-src 'unsafe-inline'"),
    );
    response
}

/// GitHub rejects a manifest whose webhook URL it cannot reach, so only a public
/// https host qualifies; otherwise the poller keeps the hub current.
fn webhook_reachable(public: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(public) else {
        return false;
    };
    let host = url
        .host_str()
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    let private_ip = |ip: IpAddr| match ip {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        // fc00::/7 unique-local and fe80::/10 link-local.
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    };
    url.scheme() == "https"
        && match host.parse::<IpAddr>() {
            Ok(ip) => !private_ip(ip),
            // Single-label and local-only names never resolve for GitHub.
            Err(_) => {
                host.contains('.')
                    && !host.ends_with(".local")
                    && !host.ends_with(".internal")
                    && !host.ends_with(".localhost")
            }
        }
}

/// Names as GitHub documents them for the manifest flow; not checked against live docs.
/// Loopback redirect and callback URLs are allowed by GitHub, so they stay.
fn manifest(name: &str, public: &str, connection: &str) -> Value {
    let mut manifest = json!({"name": name, "url": public,
        "redirect_url": format!("{public}/v1/hub/github/setup/callback"),
        "callback_urls": [format!("{public}/v1/hub/login/github/callback")],
        "public": false,
        "default_permissions": {"contents": "read", "metadata": "read",
            "pull_requests": "write", "statuses": "write"}});
    if webhook_reachable(public) {
        manifest["hook_attributes"] =
            json!({"url": format!("{public}/v1/hub/webhooks/github/{connection}")});
        manifest["default_events"] = json!(["pull_request", "push"]);
    }
    manifest
}

/// The page that posts the app manifest to GitHub. The state in the URL is the capability.
async fn setup_page(
    State(state): State<HubState>,
    Path(secret): Path<String>,
) -> ApiResult<Response> {
    let (config, _) = state.sync.configured()?;
    let (name, org, connection) = {
        let setups = lock(&state.sync.setups);
        let setup = setups
            .get(&secret, unix_now())
            .ok_or_else(ApiError::not_found)?;
        (
            setup.name.clone(),
            setup.org.clone(),
            setup.connection.clone(),
        )
    };
    let public = check_base_url(&config.public_url).map_err(|_| ApiError::invalid())?;
    let manifest = manifest(&name, &public, &connection);
    let target = match org {
        Some(org) => format!(
            "{}/organizations/{org}/settings/apps/new?state={secret}",
            config.web
        ),
        None => format!("{}/settings/apps/new?state={secret}", config.web),
    };
    Ok(page(Html(format!(
        "<!doctype html><meta charset=utf-8><title>Create the GitHub App</title>\
         <form method=post action=\"{}\"><input type=hidden name=manifest value=\"{}\">\
         <noscript><button>Continue to GitHub</button></noscript></form>\
         <script>document.forms[0].submit()</script>",
        escape(&target),
        escape(&manifest.to_string())
    ))))
}

async fn setup_callback(
    State(state): State<HubState>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let (config, api) = state.sync.configured()?;
    let params: HashMap<String, String> = reqwest::Url::parse("http://x/")
        .map(|mut url| {
            url.set_query(raw.as_deref());
            url.query_pairs().into_owned().collect()
        })
        .unwrap_or_default();
    let (Some(code), Some(secret)) = (params.get("code"), params.get("state")) else {
        return Err(ApiError::invalid());
    };
    // Single use: the state goes on any attempt.
    let setup = lock(&state.sync.setups)
        .take(secret, unix_now())
        .ok_or_else(ApiError::invalid)?;
    if setup.admin == BOOTSTRAP_ACTOR {
        let _ = std::fs::remove_file(config.secrets.join(BOOTSTRAP_FILE));
    }
    if !simple(code, b"-_", 128) {
        return Err(ApiError::invalid());
    }
    let created = api.convert_manifest(code).await.map_err(gh_to_hub)?;
    let file = AppFile {
        app_id: created["id"]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or_else(ApiError::invalid)?,
        slug: created["slug"]
            .as_str()
            .filter(|s| simple(s, b"-_", 255))
            .ok_or_else(ApiError::invalid)?
            .to_owned(),
        client_id: created["client_id"].as_str().unwrap_or_default().to_owned(),
        client_secret: created["client_secret"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        // GitHub returns no webhook secret for an app made without a webhook.
        webhook_secret: created["webhook_secret"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        pem: created["pem"].as_str().unwrap_or_default().to_owned(),
    };
    if file.webhook_secret.is_empty() && webhook_reachable(&config.public_url) {
        return Err(ApiError::invalid());
    }
    AppKey::from_pem(&file.pem)?;
    let (dir, id) = (config.secrets.clone(), setup.connection.clone());
    let text = serde_json::to_string(&file).map_err(|e| HubError::Unavailable(e.to_string()))?;
    let client_secret = file.client_secret.clone();
    blocking(&state, move || {
        prepare_secrets_dir(&dir, true)?;
        write_private(&dir.join(format!("{id}.json")), text.as_bytes())?;
        write_private(
            &dir.join(format!("{id}.client_secret")),
            client_secret.as_bytes(),
        )
    })
    .await?;
    let new = NewConnection {
        id: setup.connection.clone(),
        provider: "github".into(),
        host: config.host.clone(),
        kind: "app".into(),
        app_id: Some(file.app_id),
        app_slug: Some(file.slug.clone()),
        acts_as: None,
    };
    let actor = setup.admin;
    db(&state, move |store| {
        // A first-run link only creates the first connection, even if someone signed in meanwhile.
        // shortcut: check-then-insert, not one transaction; two hub processes finishing first-run
        // setups at the same moment could both pass. Make it transactional if hubs run replicated.
        if actor == BOOTSTRAP_ACTOR
            && !(store.list_users()?.is_empty() && store.connections()?.is_empty())
        {
            return Err(HubError::Invalid(
                "first-run setup is no longer valid".into(),
            ));
        }
        store.insert_connection(&new, &actor)
    })
    .await?;
    state.sync.load_app(&setup.connection)?;
    let install = created["html_url"]
        .as_str()
        .filter(|url| url.starts_with(&format!("{}/", config.web)))
        .map(|url| format!("{url}/installations/new"));
    let link = install.map_or_else(String::new, |url| {
        format!(" Install it: <a href=\"{0}\">{0}</a>", escape(&url))
    });
    // Public values only: the client id is not secret and the second line is a path.
    let signin = if file.client_id.is_empty() {
        String::new()
    } else {
        format!(
            "AGENTSCM_GITHUB_CLIENT_ID={}\nAGENTSCM_GITHUB_CLIENT_SECRET_FILE={}\n\
             AGENTSCM_ADMINS=github:<your GitHub login>",
            file.client_id,
            config
                .secrets
                .join(format!("{}.client_secret", setup.connection))
                .display()
        )
    };
    let hint = if signin.is_empty() {
        String::new()
    } else {
        eprintln!("hub: to enable sign-in, set these and restart:\n{signin}");
        format!(
            "<p>To enable sign-in, set these and restart the hub:</p><pre>{}</pre>",
            escape(&signin)
        )
    };
    Ok(page(Html(format!(
        "<!doctype html><meta charset=utf-8><title>GitHub App created</title><p>Created app {}.{link}</p>{hint}",
        escape(&file.slug)
    ))))
}

fn label(conn: &Connection) -> String {
    match conn.kind.as_str() {
        "app" => format!("GitHub App {}", conn.app_slug.as_deref().unwrap_or("?")),
        _ if conn.provider == "gitlab" => {
            format!(
                "GitLab token, acts as {}",
                conn.acts_as.as_deref().unwrap_or("?")
            )
        }
        _ => format!("PAT, acts as {}", conn.acts_as.as_deref().unwrap_or("?")),
    }
}

async fn list_connections(
    State(state): State<HubState>,
    human: HumanSession,
) -> ApiResult<Json<Value>> {
    human.admin()?;
    let rows = db(&state, |store| {
        store
            .connections()?
            .into_iter()
            .map(|conn| {
                let count = store.repositories(&conn.id)?.len();
                Ok((conn, count))
            })
            .collect::<Result<Vec<_>>>()
    })
    .await?;
    let connections: Vec<Value> = rows
        .iter()
        .map(|(conn, count)| {
            json!({"id": conn.id, "provider": conn.provider, "host": conn.host, "kind": conn.kind,
                "label": label(conn), "outbound": conn.outbound, "status": conn.status,
                "status_detail": conn.status_detail, "repositories": count})
        })
        .collect();
    Ok(Json(json!({"connections": connections})))
}

async fn known(state: &HubState, id: String) -> ApiResult<Connection> {
    db(state, move |store| store.connection(&id))
        .await?
        .ok_or_else(ApiError::not_found)
}

async fn refresh_connection(
    State(state): State<HubState>,
    human: HumanSession,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    human.admin()?;
    state.sync.configured()?;
    let conn = known(&state, id).await?;
    let reports = refresh(&state, &conn.id).await?;
    let reports: Vec<Value> = reports.iter().map(PollReport::to_json).collect();
    Ok(Json(json!({"reports": reports})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Outbound {
    enabled: bool,
}

async fn set_outbound(
    State(state): State<HubState>,
    human: HumanSession,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<Outbound>,
) -> ApiResult<StatusCode> {
    let actor = human.admin()?.user.id.clone();
    let conn = known(&state, id).await?;
    db(&state, move |store| {
        store.set_connection_outbound(&conn.id, body.enabled, &actor)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRepository {
    full_name: String,
}

async fn add_repository(
    State(state): State<HubState>,
    human: HumanSession,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<NewRepository>,
) -> ApiResult<Json<Value>> {
    human.admin()?;
    state.sync.configured()?;
    validate_repository(&body.full_name)?;
    let conn = known(&state, id).await?;
    // GitLab projects can sit in nested groups; GitHub's are owner/name.
    let depth_ok = conn.provider == "gitlab" || body.full_name.split('/').count() == 2;
    if conn.kind != "pat" || !depth_ok {
        return Err(ApiError::invalid());
    }
    let token = state
        .sync
        .token(&conn, None)
        .await
        .map_err(|e| forge_to_hub(who(&conn), e))?;
    let api = state
        .sync
        .forge(&conn)
        .map_err(|e| forge_to_hub(who(&conn), e))?;
    let info = match api.repo(&token, &body.full_name).await {
        Ok(Some(info)) => info,
        Ok(None) | Err(GhError::Auth(_)) => return Err(forbidden()),
        Err(error) => return Err(forge_to_hub(who(&conn), error).into()),
    };
    let (id, host) = (conn.id.clone(), conn.host.clone());
    let repo = db(&state, move |store| {
        store.upsert_repository(
            &id,
            &host,
            info.repository_id,
            &info.full_name,
            info.node_id.as_deref(),
            None,
        )
    })
    .await?;
    // Another connection got there first and keeps the repository.
    if repo.connection != conn.id {
        return Err(ApiError::new(StatusCode::CONFLICT, "already_connected"));
    }
    Ok(Json(
        json!({"id": repo.id, "full_name": repo.full_name, "repository_id": repo.repository_id}),
    ))
}

async fn work(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
) -> ApiResult<Json<Value>> {
    scope(&principal, &repo)?;
    let host = principal.owner.identity.host.clone();
    let (name, name_for_intents) = (repo.clone(), repo);
    let repository_id = principal.repository_id;
    let (intents, prs, branches, connected) = db(&state, move |store| {
        // By the forge's id when the token has one: a renamed or re-created name never serves another repository.
        let intents = store.live_intents(&name_for_intents, unix_now(), MAX_LIVE_INTENTS + 1)?;
        Ok(match find_repository(store, &host, &name, repository_id)? {
            Some(found) => (
                intents,
                store.observations(&found.id, "pull_request")?,
                store.observations(&found.id, "branch")?,
                Some(found.id),
            ),
            None => (intents, Vec::new(), Vec::new(), None),
        })
    })
    .await?;
    let truncated_intents = intents.len() > MAX_LIVE_INTENTS;
    let parsed = |rows: &[Observation]| -> Vec<Value> {
        rows.iter()
            .filter_map(|row| serde_json::from_str(&row.data).ok())
            .collect()
    };
    let truncated = connected
        .as_ref()
        .and_then(|id| lock(&state.sync.truncated).get(id).cloned())
        .unwrap_or(Value::Null);
    let intents: Vec<Value> = intents
        .iter()
        .take(MAX_LIVE_INTENTS)
        .map(Intent::to_json)
        .collect();
    Ok(Json(
        json!({"intents": intents, "intents_truncated": truncated_intents,
        "pull_requests": parsed(&prs), "branches": parsed(&branches),
        "connected": connected.is_some(), "truncated": if truncated.is_null() { json!({}) } else { truncated }}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_declares_a_webhook_only_for_a_public_https_host() {
        let public = manifest("app", "https://hub.example.com", "con_1");
        assert_eq!(
            public["hook_attributes"]["url"],
            "https://hub.example.com/v1/hub/webhooks/github/con_1"
        );
        assert_eq!(public["default_events"], json!(["pull_request", "push"]));
        for url in [
            "http://127.0.0.1:7332",
            "http://[::1]:7332",
            "https://localhost",
            "https://127.0.0.1",
            "http://hub.example.com",
            "https://10.0.0.5",
            "https://192.168.1.2:7332",
            "https://[fd00::1]",
            "https://hub.local",
            "https://hub",
        ] {
            let local = manifest("app", url, "con_1");
            assert!(local.get("hook_attributes").is_none(), "{url}");
            assert!(local.get("default_events").is_none(), "{url}");
            // GitHub accepts loopback redirects, so these stay.
            assert_eq!(
                local["redirect_url"],
                format!("{url}/v1/hub/github/setup/callback")
            );
            assert!(local["callback_urls"][0].is_string());
        }
    }
}
