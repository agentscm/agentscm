//! Hub HTTP surface: health, bearer authentication and shared request helpers.
use std::net::SocketAddr;
use std::path::PathBuf;

use axum::extract::{FromRequest, FromRequestParts, Json, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Router, extract::rejection::JsonRejection};
use serde::de::DeserializeOwned;
use serde_json::json;

use super::login::{Login, LoginConfig};
use super::store::{self, Principal, agent_token_digest, unix_now};
use super::sync::{self, Sync as GithubSync, SyncConfig};
use super::{HubError, HubState, Result, open_objects, router};
use crate::team::http::shutdown_signal;

pub struct HubConfig {
    pub database_url: String,
    pub objects: PathBuf,
    pub bind: SocketAddr,
    pub public: bool,
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
        .route("/v1/hub/whoami", get(whoami))
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
    let state = HubState::new(store, objects)
        .with_login(login)
        .with_sync(github);
    sync::start(&state).await?;
    if let Err(error) = sync::bootstrap(&state, signin, |name| std::env::var(name).ok()).await {
        eprintln!("hub: first-run setup link not created: {error}");
    }
    tokio::spawn(sync::poll_loop(state.clone(), config.poll_seconds));
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(crate::Error::Io)?;
    Ok(())
}

pub struct ApiError {
    status: StatusCode,
    code: &'static str,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    pub fn invalid() -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "invalid_request",
        }
    }

    pub fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "not_found",
        }
    }

    pub fn read_only() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "read_only",
        }
    }

    pub fn unauthorized() -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            code: "unauthorized",
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
            },
            HubError::Unavailable(detail) => {
                eprintln!("hub error: {detail}");
                Self {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    code: "unavailable",
                }
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({"error": self.code}))).into_response();
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

/// Like [`Authenticated`] but also accepts a read-only token: use it only on pure reads.
pub struct Reader(pub Principal);

impl FromRequestParts<HubState> for Reader {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &HubState,
    ) -> std::result::Result<Self, ApiError> {
        let digest = agent_token_digest(bearer(parts)?)?;
        let store = state.store.clone();
        blocking(state, move || store.authenticate(&digest, unix_now()))
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

async fn whoami(Reader(principal): Reader) -> Json<serde_json::Value> {
    let owner = principal.owner;
    Json(json!({
        "token": {
            "id": principal.token_id,
            "name": principal.name,
            "repository": principal.repository,
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
}
