//! Multi-user coordination hub: storage and authenticated HTTP.
pub mod advice;
pub mod connection;
pub mod coordination;
mod coordination_http;
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
pub mod onboarding;
pub mod publish;
pub mod session;
pub mod skill_workflow;
pub mod store;
mod store_pg;
mod store_sqlite;
pub mod sync;
mod ui;
pub mod view_settings;
pub mod work;
pub mod work_http;
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

#[derive(Clone)]
pub struct HubState {
    pub store: Arc<dyn Store>,
    pub objects: Arc<dyn ImmutableBackend>,
    /// Bounds concurrent blocking store work for the whole process.
    permits: Arc<Semaphore>,
    login: Arc<login::Login>,
    sync: Arc<sync::Sync>,
    skill_workflows: Arc<skill_workflow::SkillWorkflowRegistry>,
    onboarding: Arc<onboarding::Observations>,
    /// Set by `agentscm demo`; health says so, for the status page.
    demo: bool,
    workspace: Option<Arc<workspace::Workspace>>,
    #[cfg(unix)]
    workspace_execution: Option<Arc<workspace_execution::ExecutionService>>,
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
            onboarding: Arc::new(onboarding::Observations::default()),
            demo: false,
            workspace: None,
            #[cfg(unix)]
            workspace_execution: None,
        }
    }

    pub fn with_login(mut self, login: login::Login) -> Self {
        self.login = Arc::new(login);
        self
    }

    pub fn with_demo(mut self) -> Self {
        self.demo = true;
        self
    }

    pub fn with_sync(mut self, sync: sync::Sync) -> Self {
        self.sync = Arc::new(sync);
        self
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
        .merge(workspace_publication::status_routes())
        .merge(workspace_source_http::routes());
    #[cfg(not(unix))]
    let workspace_routes = workspace::routes();
    let router = Router::new()
        .merge(http::routes())
        .merge(onboarding::routes())
        .merge(work_http::routes())
        .merge(intent::routes())
        .merge(hook::routes())
        .merge(login::routes())
        .merge(mcp::routes())
        .merge(publish::routes())
        .merge(sync::routes())
        .merge(ui::routes())
        .merge(workspace_routes)
        .fallback(http::not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(axum::middleware::from_fn(request_timeout));
    // Merged after the global timeout layer, so these routes use their own bound.
    #[cfg(unix)]
    let router = router.merge(
        workspace_publication::routes()
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
            .layer(axum::middleware::from_fn(
                workspace_publication::processing_timeout,
            )),
    );
    router.with_state(state)
}

/// Longest any request may run; the notices long-poll caps at 25 seconds and uploads at 8 MiB.
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
