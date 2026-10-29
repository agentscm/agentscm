//! Multi-user coordination hub: storage and authenticated HTTP.
pub mod advice;
pub mod catchup;
mod catchup_sync;
pub mod change_record;
pub mod change_record_http;
pub mod connection;
pub mod coordination;
mod coordination_http;
pub mod device;
pub mod external_runs;
pub mod forge_api;
#[cfg(unix)]
pub mod forge_check;
pub mod forge_http;
pub mod github;
pub mod gitlab;
pub mod handoff;
pub mod hook;
pub mod http;
pub mod insights;
pub mod intent;
pub mod login;
pub mod mcp;
pub mod narrative;
pub mod onboarding;
#[cfg(unix)]
pub mod pipeline_rules;
pub mod promise;
mod promise_http;
pub mod publish;
pub mod session;
pub mod skill_workflow;
pub mod skills;
pub mod slack;
pub mod slack_protocol;
pub mod store;
mod store_pg;
mod store_sqlite;
pub mod sync;
pub mod ui;
pub mod view_settings;
pub mod work;
pub mod work_http;
pub mod work_preflight;
pub mod workspace;
#[cfg(unix)]
pub mod workspace_agent_tasks;
#[cfg(unix)]
pub mod workspace_execution;
#[cfg(unix)]
pub mod workspace_http;
#[cfg(unix)]
pub mod workspace_publication;
#[cfg(unix)]
pub mod workspace_source_http;

use std::fmt;
use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use tokio::sync::Semaphore;

use crate::backend::{ImmutableBackend, LocalBackend};
use crate::backend_s3::{S3Backend, S3Config};

pub use store::{ForgeIdentity, HubEvent, NewAgentToken, Principal, Store, TokenDigest, User};

const MAX_BODY_BYTES: usize = 64 * 1024;
const MAX_CONCURRENT_STORE_CALLS: usize = 32;

#[derive(Debug)]
pub enum HubError {
    Invalid(String),
    Unauthorized,
    /// Too many concurrent store operations; the request is not queued.
    Busy,
    /// The repository's retained work budget needs operator maintenance.
    WorkCapacity,
    /// The requested lead scope intersects durable unfinished work.
    WorkOverlap(Vec<work::WorkScopeOverlap>),
    /// A rejected scope mutation that durably created or reused a coordination request.
    WorkCoordinationRequired {
        overlaps: Vec<work::WorkScopeOverlap>,
        request: Box<coordination::CoordinationRequest>,
    },
    /// The bounded overlap scan could not prove that the requested scope is free.
    WorkOverlapIncomplete,
    /// Atomic admission found that an explicitly supplied source/merge preflight is not clear.
    /// The structured evidence is returned to the caller and no new Work is claimed.
    WorkPreflight(Box<work_preflight::WorkPreflightResult>),
    /// Internal detail for the operator's stderr only; never sent to a client.
    Unavailable(String),
}

pub type Result<T> = std::result::Result<T, HubError>;

impl fmt::Display for HubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid: {message}"),
            Self::Unauthorized => write!(f, "authentication required"),
            Self::Busy => write!(f, "busy"),
            Self::WorkCapacity => write!(
                f,
                "repository work storage capacity reached; operator compaction required"
            ),
            Self::WorkOverlap(_) => write!(f, "work scope overlaps unfinished work"),
            Self::WorkCoordinationRequired { .. } => {
                write!(f, "work scope needs a durable coordination response")
            }
            Self::WorkOverlapIncomplete => {
                write!(f, "work overlap scan is incomplete; claim refused")
            }
            Self::WorkPreflight(_) => write!(f, "work preflight requires review; claim refused"),
            Self::Unavailable(message) => write!(f, "unavailable: {message}"),
        }
    }
}

impl std::error::Error for HubError {}

impl From<rusqlite::Error> for HubError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Unavailable(error.to_string())
    }
}

impl From<postgres::Error> for HubError {
    fn from(error: postgres::Error) -> Self {
        Self::Unavailable(error.to_string())
    }
}

impl From<std::io::Error> for HubError {
    fn from(error: std::io::Error) -> Self {
        Self::Unavailable(error.to_string())
    }
}

impl From<crate::Error> for HubError {
    fn from(error: crate::Error) -> Self {
        Self::Unavailable(error.to_string())
    }
}

/// Automatic compaction keeps a week of stand-up history; the operator command can choose any
/// cutoff at least 24 hours old.
pub const WORK_AUTO_COMPACTION_AGE_SECONDS: i64 = 7 * 86_400;
const WORK_COMPACTION_FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(60);
const WORK_COMPACTION_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60 * 60);
const WORK_COMPACTION_BUSY_RETRY: std::time::Duration = std::time::Duration::from_secs(5);
const WORK_COMPACTION_BUSY_ATTEMPTS: usize = 12;

/// Applies the operator `work compact` to every repository scope with history older than
/// [`WORK_AUTO_COMPACTION_AGE_SECONDS`], with that command's own guards. Returns the applied
/// results and how many scopes were skipped: refused by a guard, such as an unacknowledged
/// delivery, or the store stayed busy. Each scope is a separate bounded store call.
pub async fn compact_work_history(
    state: &HubState,
    now: i64,
) -> Result<(Vec<work::WorkCompaction>, usize)> {
    let store = state.store.clone();
    let scopes = blocking_when_free(state, move || store.work_compaction_scopes()).await?;
    let before = now - WORK_AUTO_COMPACTION_AGE_SECONDS;
    let mut applied = Vec::new();
    let mut skipped = 0;
    for scope in scopes {
        let store = state.store.clone();
        match blocking_when_free(state, move || store.work_compact(&scope, before, true, now)).await
        {
            Ok(result) => applied.push(result),
            Err(HubError::Invalid(_) | HubError::Busy) => skipped += 1,
            Err(error) => return Err(error),
        }
    }
    Ok((applied, skipped))
}

async fn blocking_when_free<T: Send + 'static>(
    state: &HubState,
    work: impl FnOnce() -> Result<T> + Clone + Send + 'static,
) -> Result<T> {
    for _ in 1..WORK_COMPACTION_BUSY_ATTEMPTS {
        match http::blocking(state, work.clone()).await {
            Err(HubError::Busy) => tokio::time::sleep(WORK_COMPACTION_BUSY_RETRY).await,
            result => return result,
        }
    }
    http::blocking(state, work).await
}

/// Compacts stand-up history a minute after startup and then hourly, logging one line a pass.
pub async fn work_compaction_loop(state: HubState) {
    let mut ticker = tokio::time::interval_at(
        tokio::time::Instant::now() + WORK_COMPACTION_FIRST_DELAY,
        WORK_COMPACTION_INTERVAL,
    );
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match compact_work_history(&state, store::unix_now()).await {
            Ok((applied, skipped)) => {
                let sum = |field: fn(&work::WorkCompaction) -> u64| -> u64 {
                    applied.iter().map(field).sum()
                };
                eprintln!(
                    "hub: work compaction removed {} checkpoint receipts, {} work versions, {} coordination requests and {} merge observations across {} repositories ({skipped} skipped)",
                    sum(|c| c.deleted_operations),
                    sum(|c| c.deleted_versions),
                    sum(|c| c.deleted_coordination_requests),
                    sum(|c| c.deleted_merge_observations),
                    applied.len(),
                );
            }
            Err(error) => eprintln!("hub: work compaction failed: {error}"),
        }
    }
}

pub const DEFAULT_SHARED_CONVERSATION_RETENTION_DAYS: i64 = 30;
pub const MAX_SHARED_CONVERSATION_RETENTION_DAYS: i64 = 3650;

#[derive(Clone)]
pub struct HubState {
    pub store: Arc<dyn Store>,
    pub objects: Arc<dyn ImmutableBackend>,
    /// Bounds concurrent blocking store work for the whole process.
    permits: Arc<Semaphore>,
    login: Arc<login::Login>,
    sync: Arc<sync::Sync>,
    skill_workflows: Arc<skill_workflow::SkillWorkflowRegistry>,
    narratives: Arc<narrative::Narratives>,
    onboarding: Arc<onboarding::Observations>,
    /// Set by `agentscm demo`; health says so, for the status page.
    demo: bool,
    /// Days an uploaded shared-conversation body stays readable.
    shared_conversation_retention_days: i64,
    slack: Option<Arc<slack::Slack>>,
    workspace: Option<Arc<workspace::Workspace>>,
    #[cfg(unix)]
    workspace_execution: Option<Arc<workspace_execution::ExecutionService>>,
    #[cfg(unix)]
    publication_processing_timeout: std::time::Duration,
}

impl HubState {
    pub fn new(store: Arc<dyn Store>, objects: Arc<dyn ImmutableBackend>) -> Self {
        Self {
            store,
            objects,
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT_STORE_CALLS)),
            login: Arc::new(login::Login::unconfigured()),
            sync: Arc::new(sync::Sync::unconfigured()),
            skill_workflows: Arc::new(skill_workflow::SkillWorkflowRegistry::unconfigured()),
            narratives: Arc::new(narrative::Narratives::default()),
            onboarding: Arc::new(onboarding::Observations::default()),
            demo: false,
            shared_conversation_retention_days: DEFAULT_SHARED_CONVERSATION_RETENTION_DAYS,
            slack: None,
            workspace: None,
            #[cfg(unix)]
            workspace_execution: None,
            #[cfg(unix)]
            publication_processing_timeout: workspace_publication::PROCESSING_TIMEOUT,
        }
    }

    pub fn with_login(mut self, login: login::Login) -> Self {
        self.login = Arc::new(login);
        self
    }

    pub fn with_slack(mut self, slack: slack::Slack) -> Self {
        self.slack = Some(Arc::new(slack));
        self
    }

    /// Days to keep an uploaded shared-conversation body, 1 to 3650.
    pub fn with_shared_conversation_retention_days(mut self, days: i64) -> Result<Self> {
        if !(1..=MAX_SHARED_CONVERSATION_RETENTION_DAYS).contains(&days) {
            return Err(HubError::Invalid(format!(
                "shared conversation retention must be 1 to {MAX_SHARED_CONVERSATION_RETENTION_DAYS} days"
            )));
        }
        self.shared_conversation_retention_days = days;
        Ok(self)
    }

    pub fn with_demo(mut self) -> Self {
        self.demo = true;
        self
    }

    pub fn with_sync(mut self, sync: sync::Sync) -> Self {
        self.sync = Arc::new(sync);
        self
    }

    /// Accepted webhook deliveries whose job has not finished; zero once every accepted
    /// delivery has been handled.
    pub fn webhook_jobs_pending(&self) -> usize {
        self.sync.pending_webhooks()
    }

    pub fn with_skill_workflows(mut self, registry: skill_workflow::SkillWorkflowRegistry) -> Self {
        self.skill_workflows = Arc::new(registry);
        self
    }

    pub fn with_workspace(mut self, workspace: impl Into<Arc<workspace::Workspace>>) -> Self {
        self.workspace = Some(workspace.into());
        self
    }

    #[cfg(unix)]
    pub fn with_workspace_execution(
        mut self,
        execution: Arc<workspace_execution::ExecutionService>,
    ) -> Self {
        self.workspace_execution = Some(execution);
        self
    }

    /// Replaces the 120-second processing bound on Git-transport publication routes.
    #[cfg(unix)]
    pub fn with_publication_processing_timeout(mut self, limit: std::time::Duration) -> Self {
        self.publication_processing_timeout = limit;
        self
    }
}

/// S3 when `AGENTSCM_OBJECTS_URL` is set, else the local directory. Blocking:
/// run it off the async runtime.
pub fn open_objects(local: &Path) -> Result<Arc<dyn ImmutableBackend>> {
    let lookup = |name: &str| std::env::var(name).ok();
    Ok(match s3_config(&lookup)? {
        Some(config) => {
            eprintln!("objects: s3");
            Arc::new(S3Backend::open(config)?)
        }
        None => {
            eprintln!("objects: local");
            Arc::new(LocalBackend::open(local)?)
        }
    })
}

const S3_SETTINGS: [&str; 5] = [
    "AGENTSCM_S3_ENDPOINT",
    "AGENTSCM_S3_REGION",
    "AGENTSCM_S3_ACCESS_KEY_ID",
    "AGENTSCM_S3_SECRET_ACCESS_KEY",
    "AGENTSCM_S3_SECRET_ACCESS_KEY_FILE",
];

/// `AGENTSCM_OBJECTS_URL=s3://bucket[/prefix]` plus `AGENTSCM_S3_*`. Secrets come
/// from the environment or a file, never a flag; errors never echo values.
fn s3_config(lookup: &impl Fn(&str) -> Option<String>) -> Result<Option<S3Config>> {
    // Fail closed: a blank URL or stray S3 settings must not silently mean local disk.
    let url = match lookup("AGENTSCM_OBJECTS_URL") {
        Some(url) if url.is_empty() => {
            return Err(HubError::Invalid("AGENTSCM_OBJECTS_URL is empty".into()));
        }
        Some(url) => url,
        None if S3_SETTINGS.iter().any(|name| lookup(name).is_some()) => {
            return Err(HubError::Invalid(
                "AGENTSCM_S3_* settings need AGENTSCM_OBJECTS_URL".into(),
            ));
        }
        None => return Ok(None),
    };
    let lookup = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
    let rest = url.strip_prefix("s3://").ok_or_else(|| {
        HubError::Invalid("AGENTSCM_OBJECTS_URL must be s3://bucket[/prefix]".into())
    })?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    let region = lookup("AGENTSCM_S3_REGION").unwrap_or_else(|| "us-east-1".into());
    let secret_access_key = match (
        lookup("AGENTSCM_S3_SECRET_ACCESS_KEY_FILE"),
        lookup("AGENTSCM_S3_SECRET_ACCESS_KEY"),
    ) {
        (Some(_), Some(_)) => {
            return Err(HubError::Invalid(
                "set only one of AGENTSCM_S3_SECRET_ACCESS_KEY_FILE and AGENTSCM_S3_SECRET_ACCESS_KEY".into(),
            ));
        }
        (Some(path), None) => login::read_secret_file(&path, "AGENTSCM_S3_SECRET_ACCESS_KEY_FILE")?,
        (None, Some(secret)) => secret,
        (None, None) => {
            return Err(HubError::Invalid(
                "AGENTSCM_S3_SECRET_ACCESS_KEY_FILE or AGENTSCM_S3_SECRET_ACCESS_KEY is required with AGENTSCM_OBJECTS_URL".into(),
            ));
        }
    };
    let config = S3Config {
        endpoint: lookup("AGENTSCM_S3_ENDPOINT")
            .unwrap_or_else(|| format!("https://s3.{region}.amazonaws.com")),
        access_key_id: lookup("AGENTSCM_S3_ACCESS_KEY_ID").ok_or_else(|| {
            HubError::Invalid(
                "AGENTSCM_S3_ACCESS_KEY_ID is required with AGENTSCM_OBJECTS_URL".into(),
            )
        })?,
        region,
        bucket: bucket.to_owned(),
        prefix: prefix.trim_end_matches('/').to_owned(),
        secret_access_key,
    };
    config.validate()?;
    Ok(Some(config))
}

pub fn router(state: HubState) -> Router {
    #[cfg(unix)]
    let workspace_routes = workspace::routes()
        .merge(workspace_http::routes())
        .merge(workspace_agent_tasks::routes())
        .merge(workspace_publication::status_routes());
    #[cfg(not(unix))]
    let workspace_routes = workspace::routes();
    let router = Router::new()
        .merge(http::routes())
        .merge(onboarding::routes())
        .merge(slack::routes())
        .merge(change_record_http::routes())
        .merge(promise_http::routes())
        .merge(skills::routes())
        .merge(work_http::routes())
        .merge(intent::routes())
        .merge(hook::routes())
        .merge(login::routes())
        .merge(mcp::routes())
        .merge(publish::routes())
        .merge(sync::routes())
        .merge(ui::routes())
        .merge(workspace_routes)
        .fallback(http::not_found);
    #[cfg(unix)]
    let publication = workspace_publication::routes();
    #[cfg(not(unix))]
    let publication = Router::new();
    let router = bounded(router, publication, &state);
    #[cfg(unix)]
    let router = router.merge(source_bounded(workspace_source_http::routes()));
    router.with_state(state)
}

/// Source bundles may take [`workspace_source_http::BODY_TIMEOUT`] to arrive, longer than the
/// Hub-wide bound, so these routes get the body read plus the usual processing time.
#[cfg(unix)]
fn source_bounded(routes: Router<HubState>) -> Router<HubState> {
    routes
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn(|request, next| {
            timeout_after(
                workspace_source_http::BODY_TIMEOUT + REQUEST_TIMEOUT,
                request,
                next,
            )
        }))
}

/// Applies the Hub-wide body and time bounds to `routes`, then merges `publication` behind
/// its own longer processing bound. Routes merged after a layer are not wrapped by it.
fn bounded(
    routes: Router<HubState>,
    publication: Router<HubState>,
    state: &HubState,
) -> Router<HubState> {
    let routes = routes
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn(request_timeout));
    #[cfg(not(unix))]
    let _ = state;
    #[cfg(unix)]
    let publication = publication
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            workspace_publication::processing_timeout,
        ));
    routes.merge(publication)
}

/// Longest a request outside the publication and source groups may run; the notices long-poll caps at 25 seconds and uploads at 8 MiB.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

async fn request_timeout(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    timeout_after(REQUEST_TIMEOUT, request, next).await
}

async fn timeout_after(
    limit: std::time::Duration,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    tokio::time::timeout(limit, next.run(request))
        .await
        .unwrap_or_else(|_| axum::http::StatusCode::REQUEST_TIMEOUT.into_response())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    #[tokio::test]
    async fn a_handler_past_the_limit_gets_408() {
        let limit = std::time::Duration::from_millis(50);
        let app = Router::new()
            .route(
                "/slow",
                axum::routing::get(|| async {
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await
                }),
            )
            .route("/fast", axum::routing::get(|| async {}))
            .layer(axum::middleware::from_fn(move |request, next| {
                timeout_after(limit, request, next)
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await });
        let status = |path: &str| {
            let url = format!("http://{address}{path}");
            async move { reqwest::get(url).await.unwrap().status().as_u16() }
        };
        assert_eq!(status("/slow").await, 408);
        assert_eq!(status("/fast").await, 200);
    }

    /// A source upload may read its body for 60 seconds, so the 30-second Hub bound must not
    /// wrap it; every other route still gets 408 past 30 seconds.
    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn source_routes_outlast_the_hub_timeout() {
        use tower_service::Service;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.sqlite");
        let store = store::open(&format!("sqlite://{}", path.display())).unwrap();
        let objects = Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap());
        let state = HubState::new(store, objects);
        let slow = || {
            Router::new().route(
                "/slow",
                axum::routing::get(|| async {
                    tokio::time::sleep(REQUEST_TIMEOUT + std::time::Duration::from_secs(10)).await
                }),
            )
        };
        let status = |router: Router<HubState>| {
            let mut app = router.with_state(state.clone());
            async move {
                let request = axum::extract::Request::get("/slow")
                    .body(axum::body::Body::empty())
                    .unwrap();
                app.call(request).await.unwrap().status().as_u16()
            }
        };
        assert_eq!(status(bounded(slow(), Router::new(), &state)).await, 408);
        assert_eq!(status(source_bounded(slow())).await, 200);
    }

    fn config(extra: &[(&str, &str)]) -> Result<Option<S3Config>> {
        let mut vars: HashMap<String, String> = [
            ("AGENTSCM_OBJECTS_URL", "s3://objects/team/a"),
            ("AGENTSCM_S3_ACCESS_KEY_ID", "key-id"),
            ("AGENTSCM_S3_SECRET_ACCESS_KEY", "secret-value"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        for (name, value) in extra {
            if value.is_empty() {
                vars.remove(*name);
            } else {
                vars.insert((*name).to_owned(), (*value).to_owned());
            }
        }
        s3_config(&|name| vars.get(name).cloned())
    }

    #[test]
    fn parses_the_s3_url_and_defaults() {
        let config = config(&[]).unwrap().unwrap();
        assert_eq!(
            (
                config.bucket.as_str(),
                config.prefix.as_str(),
                config.region.as_str()
            ),
            ("objects", "team/a", "us-east-1")
        );
        assert_eq!(config.endpoint, "https://s3.us-east-1.amazonaws.com");
        let config = self::config(&[("AGENTSCM_OBJECTS_URL", "s3://objects/")])
            .unwrap()
            .unwrap();
        assert_eq!(config.prefix, "");
        assert!(s3_config(&|_| None).unwrap().is_none());
        let stray = self::config(&[("AGENTSCM_OBJECTS_URL", "")]).err().unwrap();
        assert!(stray.to_string().contains("need AGENTSCM_OBJECTS_URL"));
        let blank = |name: &str| (name == "AGENTSCM_OBJECTS_URL").then(String::new);
        assert!(s3_config(&blank).is_err());
    }

    #[test]
    fn refuses_unsafe_or_incomplete_s3_settings() {
        for (name, value) in [
            ("AGENTSCM_OBJECTS_URL", "https://objects"),
            ("AGENTSCM_OBJECTS_URL", "s3://Bad_Bucket"),
            ("AGENTSCM_OBJECTS_URL", "s3://objects/a/../b"),
            ("AGENTSCM_S3_ENDPOINT", "http://s3.example.com"),
            ("AGENTSCM_S3_SECRET_ACCESS_KEY", ""),
            ("AGENTSCM_S3_ACCESS_KEY_ID", ""),
        ] {
            let error = config(&[(name, value)]).err().unwrap().to_string();
            assert!(
                !error.contains("secret-value") && !error.contains("Bad_Bucket"),
                "{error}"
            );
        }
        let both = config(&[("AGENTSCM_S3_SECRET_ACCESS_KEY_FILE", "/nonexistent")]);
        assert!(both.err().unwrap().to_string().contains("only one of"));
    }
}
