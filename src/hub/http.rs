//! Hub HTTP surface: health, bearer authentication and shared request helpers.
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::rejection::QueryRejection;
use axum::extract::{FromRequest, FromRequestParts, Json, Path, Query, RawQuery, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, extract::rejection::JsonRejection};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::login::{Login, LoginConfig};
use super::skill_workflow::{ReadError as SkillWorkflowReadError, SkillWorkflowRegistry};
use super::store::{self, Principal, agent_token_digest, unix_now, web_read_token_digest};
use super::sync::{self, Sync as GithubSync, SyncConfig};
use super::{HubError, HubState, Result, open_objects, router};
use crate::team::http::shutdown_signal;

pub struct HubConfig {
    pub database_url: String,
    pub objects: PathBuf,
    pub bind: SocketAddr,
    pub public: bool,
    /// Days a shared-conversation body stays readable after upload.
    pub shared_conversation_retention_days: i64,
    /// Where browsers and forges reach this hub; default `http://<bind>`.
    pub public_url: Option<String>,
    /// Holds GitHub App credentials (mode 0700, created if absent).
    pub secrets: PathBuf,
    /// Seconds between GitHub polls, at least 10.
    pub poll_seconds: u64,
    /// How far back a branch's tip commit counts as recent.
    pub activity_days: i64,
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/health", get(health))
        // The path load balancers and `agentscm serve` use.
        .route("/health", get(health))
        .route("/v1/hub/whoami", get(whoami))
        .route("/v1/hub/view-settings", get(view_settings))
        .route("/v1/hub/skill-workflows", get(skill_workflows))
        .route("/v1/hub/skill-workflows/{run_id}", get(skill_workflow))
}

/// Run blocking store work off the async runtime, bounded process-wide: with
/// no free permit it fails with `Busy` instead of queueing.
pub async fn blocking<T: Send + 'static>(
    state: &HubState,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = state
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| HubError::Busy)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| HubError::Unavailable("blocking task failed".into()))?
}

pub async fn serve(config: HubConfig) -> Result<()> {
    if !config.bind.ip().is_loopback() && !config.public {
        return Err(HubError::Invalid(
            "binding a non-loopback address requires --public".into(),
        ));
    }
    let workflows = tokio::task::spawn_blocking(SkillWorkflowRegistry::from_env)
        .await
        .map_err(|_| HubError::Unavailable("startup task failed".into()))??;
    let url = config.database_url;
    let store = tokio::task::spawn_blocking(move || store::open(&url))
        .await
        .map_err(|_| HubError::Unavailable("startup task failed".into()))??;
    let local = config.objects;
    let objects = tokio::task::spawn_blocking(move || open_objects(&local))
        .await
        .map_err(|_| HubError::Unavailable("startup task failed".into()))??;
    let listener = tokio::net::TcpListener::bind(config.bind)
        .await
        .map_err(crate::Error::Io)?;
    eprintln!(
        "hub listening on {} (storage: {})",
        listener.local_addr().map_err(crate::Error::Io)?,
        store.backend()
    );
    let address = listener.local_addr().map_err(crate::Error::Io)?;
    let public_url = config
        .public_url
        .unwrap_or_else(|| format!("http://{address}"));
    let login = Login::new(LoginConfig::from_lookup(&public_url, |name| {
        std::env::var(name).ok()
    })?)?;
    let providers = login.enabled();
    let signin = !providers.is_empty();
    eprintln!(
        "login providers: {}",
        if signin {
            providers.join(", ")
        } else {
            "none".into()
        }
    );
    if config.poll_seconds < 10 {
        return Err(HubError::Invalid(
            "--poll-seconds must be at least 10".into(),
        ));
    }
    sync::prepare_secrets_dir(&config.secrets, false)?;
    let github = GithubSync::new(SyncConfig::from_lookup(
        &public_url,
        config.secrets,
        config.activity_days,
        |name| std::env::var(name).ok(),
    )?)?;
    let mut state = HubState::new(store.clone(), objects)
        .with_login(login)
        .with_sync(github)
        .with_skill_workflows(workflows)
        .with_shared_conversation_retention_days(config.shared_conversation_retention_days)?;
    if let Some(path) = std::env::var_os("AGENTSCM_WORKSPACE_FILE") {
        let config = super::workspace::WorkspaceConfig::from_file(PathBuf::from(path).as_path())?;
        #[cfg(unix)]
        let execution_database = config.database.with_extension("execution.sqlite");
        let workspace =
            tokio::task::spawn_blocking(move || super::workspace::Workspace::open(config, store))
                .await
                .map_err(|_| HubError::Unavailable("workspace startup failed".into()))??;
        state = state.with_workspace(workspace);
        #[cfg(unix)]
        if let Some(path) = std::env::var_os(super::workspace_execution::CONFIG_ENV) {
            let config = super::workspace_execution::ExecutionConfig::from_file(
                PathBuf::from(path).as_path(),
            )?;
            let workspace = state
                .workspace
                .clone()
                .ok_or_else(|| HubError::Unavailable("workspace startup failed".into()))?;
            let execution = tokio::task::spawn_blocking(move || {
                super::workspace_execution::ExecutionService::open(
                    &execution_database,
                    config,
                    workspace,
                )
            })
            .await
            .map_err(|_| HubError::Unavailable("execution startup failed".into()))??;
            state = state.with_workspace_execution(std::sync::Arc::new(execution));
        }
    } else if std::env::var_os("AGENTSCM_WORKSPACE_EXECUTION_FILE").is_some() {
        return Err(HubError::Invalid(
            "workspace execution requires AGENTSCM_WORKSPACE_FILE".into(),
        ));
    }
    #[cfg(not(unix))]
    if std::env::var_os("AGENTSCM_WORKSPACE_EXECUTION_FILE").is_some() {
        return Err(HubError::Invalid(
            "workspace execution is unsupported on this platform".into(),
        ));
    }
    if let Some(path) = std::env::var_os(super::slack::CONFIG_ENV).filter(|path| !path.is_empty()) {
        let slack = tokio::task::spawn_blocking(move || {
            super::slack::Slack::from_file(std::path::Path::new(&path))
        })
        .await
        .map_err(|_| HubError::Unavailable("Slack startup task failed".into()))??;
        state = state.with_slack(slack);
    }
    sync::start(&state).await?;
    if let Err(error) = sync::bootstrap(&state, signin, |name| std::env::var(name).ok()).await {
        eprintln!("hub: first-run setup link not created: {error}");
    }
    tokio::spawn(sync::poll_loop(state.clone(), config.poll_seconds));
    tokio::spawn(super::change_record_http::sweep_loop(state.clone()));
    #[cfg(unix)]
    let execution_worker = state
        .workspace_execution
        .as_ref()
        .filter(|execution| execution.server_execution_enabled())
        .map(|execution| execution.clone().spawn_worker());
    let slack_worker = state
        .slack
        .as_ref()
        .map(|slack| slack.clone().spawn_worker(state.clone()));
    crate::http_listener::serve(listener, router(state), shutdown_signal()).await;
    if let Some(worker) = slack_worker {
        worker.abort();
    }
    #[cfg(unix)]
    if let Some(worker) = execution_worker {
        worker.abort();
    }
    Ok(())
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    /// Fields added to the body: a `message` people read, and what the kind needs.
    detail: serde_json::Map<String, serde_json::Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str) -> Self {
        Self {
            status,
            code,
            detail: serde_json::Map::new(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }

    /// A one-sentence reason and fix, shown to people as is.
    pub fn with_message(self, message: impl Into<String>) -> Self {
        self.with_field("message", serde_json::Value::String(message.into()))
    }

    pub fn with_field(mut self, key: &str, value: serde_json::Value) -> Self {
        self.detail.insert(key.to_owned(), value);
        self
    }

    pub fn status(&self) -> StatusCode {
        self.status
    }

    pub fn message(&self) -> Option<&str> {
        self.detail
            .get("message")
            .and_then(serde_json::Value::as_str)
    }

    pub fn invalid() -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
            detail: serde_json::Map::new(),
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
            detail: serde_json::Map::new(),
        }
    }

    pub fn read_only() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "read_only",
            detail: serde_json::Map::new(),
        }
    }

    pub fn forbidden() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "forbidden",
            detail: serde_json::Map::new(),
        }
    }

    pub fn gone() -> Self {
        Self {
            status: StatusCode::GONE,
            code: "gone",
            detail: serde_json::Map::new(),
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
            detail: serde_json::Map::new(),
        }
    }
}

impl From<HubError> for ApiError {
    fn from(error: HubError) -> Self {
        match error {
            HubError::Invalid(_) => Self::invalid(),
            HubError::Unauthorized => Self::unauthorized(),
            HubError::Busy => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "busy",
                detail: serde_json::Map::new(),
            },
            HubError::WorkCapacity => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "work_storage_capacity",
                detail: serde_json::Map::new(),
            },
            HubError::WorkOverlap(_) => Self {
                status: StatusCode::CONFLICT,
                code: "work_scope_overlap",
                detail: serde_json::Map::new(),
            },
            HubError::WorkCoordinationRequired { .. } => Self {
                status: StatusCode::CONFLICT,
                code: "work_coordination_required",
                detail: serde_json::Map::new(),
            },
            HubError::WorkOverlapIncomplete => Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: "work_overlap_incomplete",
                detail: serde_json::Map::new(),
            },
            HubError::WorkPreflight(_) => Self {
                status: StatusCode::CONFLICT,
                code: "work_preflight_required",
                detail: serde_json::Map::new(),
            },
            HubError::Unavailable(detail) => {
                eprintln!("hub error: {detail}");
                Self {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    code: "unavailable",
                    detail: serde_json::Map::new(),
                }
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut body = self.detail;
        body.insert("error".into(), json!(self.code));
        let mut response = (self.status, Json(serde_json::Value::Object(body))).into_response();
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

pub async fn not_found() -> ApiError {
    ApiError::not_found()
}

/// JSON request body; any failure is a 400 with a fixed body. Request types
/// must use `#[serde(deny_unknown_fields)]`.
pub struct JsonBody<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for JsonBody<T> {
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> std::result::Result<Self, ApiError> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(|_: JsonRejection| ApiError::invalid())
    }
}

/// A request authenticated by `Authorization: Bearer <agent token>`. Put it
/// before `JsonBody` in handler arguments so unauthenticated callers get 401
/// before any body is parsed. Mutating handlers must recheck the token inside
/// their own write transaction: it may be revoked after this check.
/// A read-only token is refused (403 `read_only`), so every route using this is agent-only.
pub struct Authenticated(pub Principal);

impl FromRequestParts<HubState> for Authenticated {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &HubState,
    ) -> std::result::Result<Self, ApiError> {
        let Reader(principal) = Reader::from_request_parts(parts, state).await?;
        if principal.read_only {
            return Err(ApiError::read_only());
        }
        Ok(Self(principal))
    }
}

/// Like [`Authenticated`] but also accepts a read-only token. It permits reads
/// and that person's catch-up delivery/acknowledgement state, never Work claims,
/// source changes or forge mutations.
pub struct Reader(pub Principal);

impl FromRequestParts<HubState> for Reader {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &HubState,
    ) -> std::result::Result<Self, ApiError> {
        let raw = bearer(parts)?;
        let digest = agent_token_digest(raw)?;
        let web_digest = web_read_token_digest(raw)?;
        let store = state.store.clone();
        blocking(state, move || {
            let now = unix_now();
            match store.authenticate(&digest, now)? {
                Some(principal) => Ok(Some(principal)),
                None => store.authenticate(&web_digest, now),
            }
        })
        .await?
        .map(Self)
        .ok_or_else(ApiError::unauthorized)
    }
}

/// The secret from exactly one `Authorization: Bearer` header.
pub(super) fn bearer(parts: &Parts) -> std::result::Result<&str, ApiError> {
    let mut values = parts.headers.get_all(header::AUTHORIZATION).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    }
    .and_then(|value| value.split_once(' '))
    .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
    .map(|(_, token)| token)
    .ok_or_else(ApiError::unauthorized)
}

async fn health(State(state): State<HubState>) -> Response {
    let store = state.store.clone();
    match blocking(&state, move || store.ping()).await {
        Ok(()) => {
            let mut body = json!({"status": "ok", "storage": state.store.backend(),
                "coordination": super::session::coordination()});
            if state.demo {
                body["demo"] = json!(true);
            }
            Json(body).into_response()
        }
        Err(error) => {
            eprintln!("hub health check failed: {error}");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status": "unavailable"})),
            )
                .into_response()
        }
    }
}

async fn view_settings(
    State(state): State<HubState>,
    Reader(principal): Reader,
) -> std::result::Result<Json<serde_json::Value>, ApiError> {
    let store = state.store.clone();
    let report = blocking(&state, move || {
        super::view_settings::ViewSettings::report(&*store, &principal.owner.id)
    })
    .await?;
    Ok(Json(report))
}

async fn whoami(
    State(state): State<HubState>,
    Reader(principal): Reader,
) -> Json<serde_json::Value> {
    let owner = principal.owner;
    Json(json!({
        "shared_conversation_retention_days": state.shared_conversation_retention_days,
        "token": {
            "id": principal.token_id,
            "name": principal.name,
            "repository": principal.repository,
            "repository_id": principal.repository_id,
            "expires_at": principal.expires_at,
            "read_only": principal.read_only,
        },
        "owner": {
            "provider": owner.identity.provider,
            "host": owner.identity.host,
            "user_id": owner.identity.user_id,
            "login": owner.login,
        },
    }))
}

async fn skill_workflow(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(run_id): Path<String>,
    RawQuery(raw): RawQuery,
) -> std::result::Result<Response, ApiError> {
    if raw.as_deref().is_some_and(|raw| !raw.is_empty()) {
        return Err(ApiError::invalid());
    }
    let registry = state.skill_workflows.clone();
    let repository = principal.repository;
    let outcome = blocking(&state, move || Ok(registry.read(&repository, &run_id))).await?;
    match outcome {
        Ok(envelope) => Ok(Json(envelope).into_response()),
        Err(SkillWorkflowReadError::InvalidRunId) => Err(ApiError::invalid()),
        Err(SkillWorkflowReadError::NotFound) => Err(ApiError::not_found()),
        Err(SkillWorkflowReadError::Internal(detail)) => Err(HubError::Unavailable(detail).into()),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillWorkflowsQuery {
    before: Option<String>,
    q: Option<String>,
}

/// Parses a positive decimal cursor with no sign or leading zero.
pub(super) fn sequence_cursor(value: &str) -> std::result::Result<i64, ApiError> {
    if value.is_empty()
        || value.len() > 19
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ApiError::invalid());
    }
    value.parse::<i64>().map_err(|_| ApiError::invalid())
}

async fn skill_workflows(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<SkillWorkflowsQuery>, QueryRejection>,
) -> std::result::Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let before = query.before.as_deref().map(sequence_cursor).transpose()?;
    let search = super::store::search_text(query.q.as_deref())?;
    let registry = state.skill_workflows.clone();
    let repository = principal.repository;
    let outcome = blocking(&state, move || {
        Ok(registry.page(&repository, before, search.as_deref()))
    })
    .await?;
    match outcome {
        Ok(envelope) => Ok(Json(envelope).into_response()),
        Err(SkillWorkflowReadError::InvalidRunId) => Err(ApiError::invalid()),
        Err(SkillWorkflowReadError::NotFound) => Err(ApiError::not_found()),
        Err(SkillWorkflowReadError::Internal(detail)) => Err(HubError::Unavailable(detail).into()),
    }
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Strict {
        #[allow(dead_code)]
        name: String,
    }

    async fn parse(body: &str) -> std::result::Result<JsonBody<Strict>, ApiError> {
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_owned()))
            .unwrap();
        JsonBody::from_request(request, &()).await
    }

    #[tokio::test]
    async fn json_body_rejects_unknown_fields_with_400() {
        assert!(parse(r#"{"name":"a"}"#).await.is_ok());
        let error = parse(r#"{"name":"a","extra":1}"#).await.err().unwrap();
        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn work_capacity_has_an_actionable_public_error_code() {
        let response = ApiError::from(HubError::WorkCapacity).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            json!({"error": "work_storage_capacity"})
        );
    }
}
