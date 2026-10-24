//! Bounded loopback HTTP transport for repository-scoped team operations.
//!
//! This listener is intended to sit behind a trusted same-origin HTTPS proxy.
//! It does not terminate TLS, accept forwarded identity, or enable CORS.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::{Body, Bytes};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, RawQuery, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use http_body::Body as HttpBody;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::Error;
use crate::backend::LocalBackend;

use super::auth::{Credentials, NewSession};
use super::publication::{CompleteRequest, RepositoryScope, ReserveRequest, SourceNamespace};
use super::service::{
    DatabaseConfig, ProvisionRequest, RevokeRequest, RolesRequest, TeamConfig, TeamService,
};
use super::source::{SourceCancelRequest, SourcePageRequest};
use super::{Result, TeamError};

const MAX_URL_BYTES: usize = 4 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024;
const MAX_JSON_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_OBJECT_RESPONSE_BYTES: u64 = 1024 * 1024;
const RESPONSE_CHUNK_BYTES: usize = 16 * 1024;
const SESSION_COOKIE: &str = "__Host-agentscm-session";
const CSRF_HEADER: &str = "x-agentscm-csrf";

type ApiResult<T> = std::result::Result<T, ApiError>;

#[derive(Clone)]
struct AppState {
    database: DatabaseConfig,
    backend: LocalBackend,
    origin: Arc<str>,
    work: Arc<Semaphore>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
}

struct ResponseGuard {
    inner: Body,
    pending: Option<Bytes>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginRequest {
    scope: RepositoryScope,
    authority_generation: i64,
    credential: String,
    ttl_seconds: i32,
}

#[derive(Serialize)]
struct LoginResponse {
    csrf_token: String,
    expires_epoch: i64,
    principal: super::auth::PrincipalState,
    scope: RepositoryScope,
    generation: i64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileReserveRequest {
    reservation: ReserveRequest,
    profile: SourceNamespace,
}

enum NativeQuery {
    Revision,
    File(String),
    Files(SourcePageRequest),
    History(SourcePageRequest),
    Diff {
        left: String,
        right: String,
        page: SourcePageRequest,
    },
    Relation {
        left: String,
        right: String,
        page: SourcePageRequest,
    },
}

enum NativePayload {
    Json(Vec<u8>),
    File(Vec<u8>),
}

struct ScopeQuery {
    scope: RepositoryScope,
}

struct ObjectQuery {
    generation: i64,
    publication: String,
}

#[derive(Clone, Copy)]
struct ByteRange {
    offset: u64,
    length: u64,
}

/// Construct the standalone team router after validating its local storage and
/// trusted-proxy configuration. Each request still opens and closes its own
/// synchronous PostgreSQL connection inside a bounded blocking job.
pub fn router(config: TeamConfig) -> Result<Router> {
    config.validate()?;

    // Open both dependencies before a listener can be created. The database
    // handle initializes/validates the current schema and is immediately
    // dropped; the per-request service never reuses a connection or pool.
    let startup_scope = RepositoryScope {
        trust_domain: "agentscm_startup".to_owned(),
        repository: "schema_check".to_owned(),
    };
    let _ = config.database.open(startup_scope)?;
    let backend = LocalBackend::open(&config.objects)?;

    let state = AppState {
        database: config.database,
        backend,
        origin: Arc::from(config.origin),
        work: Arc::new(Semaphore::new(config.max_concurrency)),
    };

    let app = Router::new()
        .route("/v1/team/capabilities", get(capabilities))
        .route(
            "/v1/team/session",
            post(login).get(session).delete(logout),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/publications",
            post(reserve),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/publications/{generation}/{publication}",
            get(inspect_publication).delete(abandon),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/publications/{generation}/{publication}/complete",
            post(complete),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/source-publications",
            post(reserve_profile),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/publications/{generation}/{publication}/source/{view}",
            get(read_native),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/source-traversals/cancel",
            post(cancel_source),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/principals",
            post(provision),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/principals/{principal}",
            get(principal),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/principals/{principal}/roles",
            put(roles),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/principals/{principal}/revoke",
            post(revoke),
        )
        .route(
            "/v1/team/repositories/{trust_domain}/{repository}/objects/{id}",
            get(read_object),
        )
        .fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(middleware::from_fn(request_limits))
        .with_state(state);
    Ok(app)
}

/// Serve the team router on its validated loopback address until Ctrl-C (or
/// SIGTERM on Unix). HTTPS termination belongs to the configured trusted proxy.
pub async fn serve(config: TeamConfig) -> Result<()> {
    let bind = config.bind;
    let app = tokio::task::spawn_blocking(move || router(config))
        .await
        .map_err(|_| TeamError::Unavailable)??;
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .map_err(Error::Io)?;
    eprintln!(
        "team listener ready on {}",
        listener.local_addr().map_err(Error::Io)?
    );
    crate::http_listener::serve(listener, app, shutdown_signal()).await;
    Ok(())
}

async fn capabilities() -> Response {
    json_response(&json!({
        "version": 1,
        "status": "available",
        "transport_profile": "same_origin_https_proxy_to_loopback",
        "supported": [
            "repository_scoped_authentication",
            "draft_publications",
            "encoded_object_reads",
            "native_source_views",
            "explicit_r2_draft_namespaces"
        ],
        "unavailable": [
            "authenticated_upload",
            "source_qualification",
            "protected_target_admission",
            "garbage_collection",
            "recovery"
        ]
    }))
    .unwrap_or_else(|error| error.into_response())
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: std::result::Result<Json<LoginRequest>, JsonRejection>,
) -> ApiResult<Response> {
    require_origin(&headers, &state.origin)?;
    ensure_no_identity_headers(&headers)?;
    let request = json_body(body)?;
    let scope = request.scope.clone();
    let generation = request.authority_generation;
    let ttl_seconds = request.ttl_seconds;
    let database = state.database.clone();
    let (session, permit) = blocking(&state, move || {
        TeamService::login(
            &database,
            request.scope,
            &request.credential,
            request.authority_generation,
            request.ttl_seconds,
        )
    })
    .await?;
    let NewSession {
        session_token,
        csrf_token,
        expires_epoch,
        principal,
    } = session;
    let result = LoginResponse {
        csrf_token,
        expires_epoch,
        principal,
        scope,
        generation,
    };
    let mut response = json_response(&result)?;
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&session_cookie(&session_token, ttl_seconds))
            .map_err(|_| ApiError::internal())?,
    );
    Ok(guard_response(response, permit))
}

async fn session(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let query = scope_query(raw_query.as_deref())?;
    let credentials = credentials(&headers, false, &state.origin)?;
    let database = state.database.clone();
    let (view, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, query.scope, credentials)?;
        service.session()
    })
    .await?;
    Ok(guard_response(json_response(&view)?, permit))
}

async fn logout(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let query = scope_query(raw_query.as_deref())?;
    let credentials = credentials(&headers, true, &state.origin)?;
    let database = state.database.clone();
    let (_, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, query.scope, credentials)?;
        service.logout()
    })
    .await?;
    let mut response = json_response(&json!({"status": "logged_out"}))?;
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(
            "__Host-agentscm-session=; Max-Age=0; Path=/; Secure; HttpOnly; SameSite=Strict",
        ),
    );
    Ok(guard_response(response, permit))
}

async fn reserve(
    State(state): State<AppState>,
    Path((trust_domain, repository)): Path<(String, String)>,
    headers: HeaderMap,
    body: std::result::Result<Json<ReserveRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let request = json_body(body)?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, true, trust_domain, repository)?;
    let (_, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.reserve(&request)
    })
    .await?;
    Ok(guard_response(
        json_response(&json!({"status": "reserved"}))?,
        permit,
    ))
}

async fn reserve_profile(
    State(state): State<AppState>,
    Path((trust_domain, repository)): Path<(String, String)>,
    headers: HeaderMap,
    body: std::result::Result<Json<ProfileReserveRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let request = json_body(body)?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, true, trust_domain, repository)?;
    let (_, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.reserve_profile(&request.reservation, request.profile)
    })
    .await?;
    Ok(guard_response(
        json_response(&json!({"status": "reserved"}))?,
        permit,
    ))
}

async fn cancel_source(
    State(state): State<AppState>,
    Path((trust_domain, repository)): Path<(String, String)>,
    headers: HeaderMap,
    body: std::result::Result<Json<SourceCancelRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let request = json_body(body)?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, true, trust_domain, repository)?;
    let (outcome, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.source_cancel(&request)
    })
    .await?;
    Ok(guard_response(json_response(&outcome)?, permit))
}

async fn read_native(
    State(state): State<AppState>,
    Path((trust_domain, repository, generation, publication, view)): Path<(
        String,
        String,
        String,
        String,
        String,
    )>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let generation = positive_i64(&generation)?;
    if headers.get_all(header::RANGE).iter().next().is_some() {
        return Err(ApiError::invalid());
    }
    let query = native_query(&view, raw_query.as_deref())?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, false, trust_domain, repository)?;
    let backend = state.backend.clone();
    let (payload, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        match query {
            NativeQuery::Revision => {
                native_json(&service.source_revision(&backend, generation, &publication)?)
            }
            NativeQuery::File(path) => {
                let file = service.source_file(&backend, generation, &publication, &path)?;
                if file.bytes.len() as u64 > MAX_OBJECT_RESPONSE_BYTES {
                    return Err(Error::Limit("native file exceeds response bound".into()).into());
                }
                Ok(NativePayload::File(file.bytes))
            }
            NativeQuery::Files(page) => {
                native_json(&service.source_files(&backend, generation, &publication, &page)?)
            }
            NativeQuery::History(page) => {
                native_json(&service.source_history(&backend, generation, &publication, &page)?)
            }
            NativeQuery::Diff { left, right, page } => native_json(&service.source_diff(
                &backend,
                generation,
                &publication,
                &left,
                &right,
                &page,
            )?),
            NativeQuery::Relation { left, right, page } => native_json(&service.source_relation(
                &backend,
                generation,
                &publication,
                &left,
                &right,
                &page,
            )?),
        }
    })
    .await?;
    let (bytes, content_type) = match payload {
        NativePayload::Json(bytes) => (bytes, "application/json"),
        NativePayload::File(bytes) => (bytes, "application/octet-stream"),
    };
    let mut response = Response::new(Body::from(bytes));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    Ok(guard_response(response, permit))
}

fn native_json<T: Serialize>(value: &T) -> Result<NativePayload> {
    let bytes = serde_json::to_vec(value).map_err(Error::from)?;
    if bytes.len() > MAX_JSON_RESPONSE_BYTES {
        return Err(Error::Limit("native page exceeds response bound".into()).into());
    }
    Ok(NativePayload::Json(bytes))
}

async fn inspect_publication(
    State(state): State<AppState>,
    Path((trust_domain, repository, generation, publication)): Path<(
        String,
        String,
        String,
        String,
    )>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let generation = positive_i64(&generation)?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, false, trust_domain, repository)?;
    let (view, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.inspect(generation, &publication)
    })
    .await?;
    Ok(guard_response(json_response(&view)?, permit))
}

async fn abandon(
    State(state): State<AppState>,
    Path((trust_domain, repository, generation, publication)): Path<(
        String,
        String,
        String,
        String,
    )>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let generation = positive_i64(&generation)?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, true, trust_domain, repository)?;
    let (_, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.abandon(generation, &publication)
    })
    .await?;
    Ok(guard_response(
        json_response(&json!({"status": "abandoned"}))?,
        permit,
    ))
}

async fn complete(
    State(state): State<AppState>,
    Path((trust_domain, repository, generation, publication)): Path<(
        String,
        String,
        String,
        String,
    )>,
    headers: HeaderMap,
    body: std::result::Result<Json<CompleteRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let path_generation = positive_i64(&generation)?;
    let request = json_body(body)?;
    if request.generation != path_generation || request.publication != publication {
        return Err(ApiError::invalid());
    }
    let (database, scope, credentials) =
        service_inputs(&state, &headers, true, trust_domain, repository)?;
    let (outcome, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.complete(&request)
    })
    .await?;
    Ok(guard_response(json_response(&outcome)?, permit))
}

async fn provision(
    State(state): State<AppState>,
    Path((trust_domain, repository)): Path<(String, String)>,
    headers: HeaderMap,
    body: std::result::Result<Json<ProvisionRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let request = json_body(body)?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, true, trust_domain, repository)?;
    let (outcome, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.provision(&request)
    })
    .await?;
    Ok(guard_response(json_response(&outcome)?, permit))
}

async fn principal(
    State(state): State<AppState>,
    Path((trust_domain, repository, principal)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (database, scope, credentials) =
        service_inputs(&state, &headers, false, trust_domain, repository)?;
    let (view, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.principal(&principal)
    })
    .await?;
    Ok(guard_response(json_response(&view)?, permit))
}

async fn roles(
    State(state): State<AppState>,
    Path((trust_domain, repository, principal)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: std::result::Result<Json<RolesRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let request = json_body(body)?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, true, trust_domain, repository)?;
    let (outcome, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.roles(&principal, &request)
    })
    .await?;
    Ok(guard_response(json_response(&outcome)?, permit))
}

async fn revoke(
    State(state): State<AppState>,
    Path((trust_domain, repository, principal)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: std::result::Result<Json<RevokeRequest>, JsonRejection>,
) -> ApiResult<Response> {
    let request = json_body(body)?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, true, trust_domain, repository)?;
    let (outcome, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.revoke(&principal, &request)
    })
    .await?;
    Ok(guard_response(json_response(&outcome)?, permit))
}

async fn read_object(
    State(state): State<AppState>,
    Path((trust_domain, repository, id)): Path<(String, String, String)>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let query = object_query(raw_query.as_deref())?;
    let requested_range = range(&headers)?;
    let (database, scope, credentials) =
        service_inputs(&state, &headers, false, trust_domain, repository)?;
    let backend = state.backend.clone();
    let (read, permit) = blocking(&state, move || {
        let mut service = TeamService::connect(&database, scope, credentials)?;
        service.read_object(
            &backend,
            query.generation,
            &query.publication,
            &id,
            requested_range.map_or(0, |value| value.offset),
            requested_range.map(|value| value.length),
        )
    })
    .await?;
    if read.bytes.len() as u64 > MAX_OBJECT_RESPONSE_BYTES {
        return Err(ApiError::limit());
    }
    let response_bytes = read.bytes.len() as u64;
    let mut response = Response::new(Body::from(read.bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    if requested_range.is_some() {
        let end = read
            .offset
            .checked_add(response_bytes)
            .and_then(|value| value.checked_sub(1))
            .ok_or_else(ApiError::invalid)?;
        *response.status_mut() = StatusCode::PARTIAL_CONTENT;
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!(
                "bytes {}-{}/{}",
                read.offset, end, read.total_bytes
            ))
            .map_err(|_| ApiError::internal())?,
        );
    }
    Ok(guard_response(response, permit))
}

fn service_inputs(
    state: &AppState,
    headers: &HeaderMap,
    mutation: bool,
    trust_domain: String,
    repository: String,
) -> ApiResult<(DatabaseConfig, RepositoryScope, Credentials)> {
    Ok((
        state.database.clone(),
        RepositoryScope {
            trust_domain,
            repository,
        },
        credentials(headers, mutation, &state.origin)?,
    ))
}

async fn blocking<T, F>(state: &AppState, operation: F) -> ApiResult<(T, OwnedSemaphorePermit)>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    let permit = state
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::unavailable())?;
    tokio::task::spawn_blocking(move || operation().map(|value| (value, permit)))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(ApiError::from)
}

async fn request_limits(request: Request, next: Next) -> Response {
    let url_bytes = request
        .uri()
        .path_and_query()
        .map_or(0, |value| value.as_str().len());
    if url_bytes > MAX_URL_BYTES || header_bytes(request.headers()).is_none() {
        return ApiError::limit().into_response();
    }
    let response = next.run(request).await;
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if (response.status().is_client_error() || response.status().is_server_error()) && !is_json {
        return framework_error(response.status()).into_response();
    }
    response
}

fn framework_error(status: StatusCode) -> ApiError {
    match status {
        StatusCode::BAD_REQUEST | StatusCode::UNPROCESSABLE_ENTITY => ApiError::invalid(),
        StatusCode::UNAUTHORIZED => ApiError::unauthorized(),
        StatusCode::FORBIDDEN => ApiError::denied(),
        StatusCode::NOT_FOUND => ApiError::new(status, "not_found"),
        StatusCode::METHOD_NOT_ALLOWED => ApiError::new(status, "method_not_allowed"),
        StatusCode::CONFLICT => ApiError::new(status, "conflict"),
        StatusCode::GONE => ApiError::new(status, "expired"),
        StatusCode::PAYLOAD_TOO_LARGE => ApiError::limit(),
        StatusCode::SERVICE_UNAVAILABLE => ApiError::unavailable(),
        _ => ApiError::internal(),
    }
}

fn header_bytes(headers: &HeaderMap) -> Option<usize> {
    headers.iter().try_fold(0usize, |total, (name, value)| {
        total
            .checked_add(name.as_str().len())?
            .checked_add(value.as_bytes().len())?
            .checked_add(4)
            .filter(|value| *value <= MAX_HEADER_BYTES)
    })
}

fn credentials(headers: &HeaderMap, mutation: bool, origin: &str) -> ApiResult<Credentials> {
    let bearer = bearer(headers)?;
    let browser = session_token(headers)?;
    if bearer.is_some() && browser.is_some() {
        return Err(ApiError::unauthorized());
    }
    if let Some(raw) = bearer {
        return Credentials::bearer(raw).map_err(ApiError::from);
    }
    let raw = browser.ok_or_else(ApiError::unauthorized)?;
    let csrf = single_header(headers, CSRF_HEADER)?;
    if mutation {
        require_origin(headers, origin)?;
        if csrf.is_none() {
            return Err(ApiError::unauthorized());
        }
    }
    Credentials::browser(raw, csrf).map_err(ApiError::from)
}

fn bearer(headers: &HeaderMap) -> ApiResult<Option<&str>> {
    let values: Vec<_> = headers.get_all(header::AUTHORIZATION).iter().collect();
    if values.len() > 1 {
        return Err(ApiError::unauthorized());
    }
    let Some(value) = values.first() else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ApiError::unauthorized())?;
    let raw = value
        .strip_prefix("Bearer ")
        .filter(|raw| !raw.is_empty() && !raw.contains(' '))
        .ok_or_else(ApiError::unauthorized)?;
    Ok(Some(raw))
}

fn session_token(headers: &HeaderMap) -> ApiResult<Option<&str>> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE).iter() {
        let value = value.to_str().map_err(|_| ApiError::unauthorized())?;
        for field in value.split(';') {
            let Some((name, raw)) = field.trim().split_once('=') else {
                continue;
            };
            if name == SESSION_COOKIE {
                if found.is_some() || raw.is_empty() {
                    return Err(ApiError::unauthorized());
                }
                found = Some(raw);
            }
        }
    }
    Ok(found)
}

fn ensure_no_identity_headers(headers: &HeaderMap) -> ApiResult<()> {
    if headers
        .get_all(header::AUTHORIZATION)
        .iter()
        .next()
        .is_some()
    {
        return Err(ApiError::unauthorized());
    }
    // A revoked/expired browser session must not prevent a fresh credential
    // exchange. Origin is checked first; a single old cookie is replaced, not
    // trusted as a login identity. Still reject duplicate identity cookies.
    let _ = session_token(headers)?;
    Ok(())
}

fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<Option<&'a str>> {
    let values: Vec<_> = headers.get_all(name).iter().collect();
    if values.len() > 1 {
        return Err(ApiError::invalid());
    }
    values
        .first()
        .map(|value| value.to_str().map_err(|_| ApiError::invalid()))
        .transpose()
}

fn require_origin(headers: &HeaderMap, expected: &str) -> ApiResult<()> {
    match single_header(headers, header::ORIGIN.as_str())? {
        Some(actual) if actual.as_bytes() == expected.as_bytes() => Ok(()),
        _ => Err(ApiError::denied()),
    }
}

fn scope_query(raw: Option<&str>) -> ApiResult<ScopeQuery> {
    let fields = query_fields(raw)?;
    let mut trust_domain = None;
    let mut repository = None;
    for (name, value) in fields {
        match name.as_str() {
            "trust_domain" if trust_domain.is_none() => trust_domain = Some(value),
            "repository" if repository.is_none() => repository = Some(value),
            _ => return Err(ApiError::invalid()),
        }
    }
    Ok(ScopeQuery {
        scope: RepositoryScope {
            trust_domain: trust_domain.ok_or_else(ApiError::invalid)?,
            repository: repository.ok_or_else(ApiError::invalid)?,
        },
    })
}

fn object_query(raw: Option<&str>) -> ApiResult<ObjectQuery> {
    let fields = query_fields(raw)?;
    let mut generation = None;
    let mut publication = None;
    for (name, value) in fields {
        match name.as_str() {
            "generation" if generation.is_none() => generation = Some(positive_i64(&value)?),
            "publication" if publication.is_none() => publication = Some(value),
            _ => return Err(ApiError::invalid()),
        }
    }
    Ok(ObjectQuery {
        generation: generation.ok_or_else(ApiError::invalid)?,
        publication: publication.ok_or_else(ApiError::invalid)?,
    })
}

fn native_query(view: &str, raw: Option<&str>) -> ApiResult<NativeQuery> {
    let fields = match raw {
        None | Some("") => Vec::new(),
        Some(raw) => query_fields(Some(raw))?,
    };
    let (mut path, mut left, mut right, mut cursor, mut limit, mut operation) =
        (None, None, None, None, None, None);
    for (name, value) in fields {
        match name.as_str() {
            "path" if path.is_none() => path = Some(value),
            "left" if left.is_none() => left = Some(value),
            "right" if right.is_none() => right = Some(value),
            "cursor" if cursor.is_none() => cursor = Some(value),
            "operation" if operation.is_none() => {
                super::publication::identifier(&value).map_err(|_| ApiError::invalid())?;
                operation = Some(value);
            }
            "limit" if limit.is_none() => {
                let value = value.parse::<usize>().map_err(|_| ApiError::invalid())?;
                if !(1..=256).contains(&value) {
                    return Err(ApiError::limit());
                }
                limit = Some(value);
            }
            _ => return Err(ApiError::invalid()),
        }
    }
    match view {
        "revision"
            if path.is_none()
                && left.is_none()
                && right.is_none()
                && cursor.is_none()
                && limit.is_none()
                && operation.is_none() =>
        {
            Ok(NativeQuery::Revision)
        }
        "file"
            if left.is_none()
                && right.is_none()
                && cursor.is_none()
                && limit.is_none()
                && operation.is_none() =>
        {
            Ok(NativeQuery::File(path.ok_or_else(ApiError::invalid)?))
        }
        "files" | "history" if path.is_none() && left.is_none() && right.is_none() => {
            let page = SourcePageRequest {
                operation: operation.ok_or_else(ApiError::invalid)?,
                cursor,
                limit: limit.unwrap_or(128),
            };
            if view == "files" {
                Ok(NativeQuery::Files(page))
            } else {
                Ok(NativeQuery::History(page))
            }
        }
        "diff" | "relation" if path.is_none() => {
            let left = left.ok_or_else(ApiError::invalid)?;
            let right = right.ok_or_else(ApiError::invalid)?;
            let page = SourcePageRequest {
                operation: operation.ok_or_else(ApiError::invalid)?,
                cursor,
                limit: limit.unwrap_or(128),
            };
            if view == "diff" {
                Ok(NativeQuery::Diff { left, right, page })
            } else {
                Ok(NativeQuery::Relation { left, right, page })
            }
        }
        _ => Err(ApiError::invalid()),
    }
}

fn query_fields(raw: Option<&str>) -> ApiResult<Vec<(String, String)>> {
    let raw = raw.ok_or_else(ApiError::invalid)?;
    if raw.is_empty() || raw.len() > MAX_URL_BYTES {
        return Err(ApiError::invalid());
    }
    raw.split('&')
        .map(|field| {
            let (name, value) = field.split_once('=').ok_or_else(ApiError::invalid)?;
            Ok((decode_query(name)?, decode_query(value)?))
        })
        .collect()
}

fn decode_query(input: &str) -> ApiResult<String> {
    let mut bytes = Vec::with_capacity(input.len());
    let input = input.as_bytes();
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            b'%' if index + 2 < input.len() => {
                bytes.push((hex(input[index + 1])? << 4) | hex(input[index + 2])?);
                index += 3;
            }
            b'%' => return Err(ApiError::invalid()),
            b'+' => {
                bytes.push(b' ');
                index += 1;
            }
            byte => {
                bytes.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(bytes).map_err(|_| ApiError::invalid())
}

fn hex(byte: u8) -> ApiResult<u8> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ApiError::invalid()),
    }
}

fn range(headers: &HeaderMap) -> ApiResult<Option<ByteRange>> {
    let values: Vec<_> = headers.get_all(header::RANGE).iter().collect();
    if values.len() > 1 {
        return Err(ApiError::invalid());
    }
    let Some(value) = values.first() else {
        return Ok(None);
    };
    let value = value.to_str().map_err(|_| ApiError::invalid())?;
    let value = value.strip_prefix("bytes=").ok_or_else(ApiError::invalid)?;
    if value.contains(',') {
        return Err(ApiError::invalid());
    }
    let (start, end) = value.split_once('-').ok_or_else(ApiError::invalid)?;
    if start.is_empty() || end.is_empty() {
        return Err(ApiError::invalid());
    }
    let offset = start.parse::<u64>().map_err(|_| ApiError::invalid())?;
    let end = end.parse::<u64>().map_err(|_| ApiError::invalid())?;
    let length = end
        .checked_sub(offset)
        .and_then(|value| value.checked_add(1))
        .ok_or_else(ApiError::invalid)?;
    if length > MAX_OBJECT_RESPONSE_BYTES {
        return Err(ApiError::limit());
    }
    Ok(Some(ByteRange { offset, length }))
}

fn positive_i64(value: &str) -> ApiResult<i64> {
    value
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(ApiError::invalid)
}

fn json_body<T>(body: std::result::Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    body.map(|Json(value)| value).map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::limit()
        } else {
            ApiError::invalid()
        }
    })
}

fn json_response<T: Serialize>(value: &T) -> ApiResult<Response> {
    let bytes = serde_json::to_vec(value).map_err(|_| ApiError::internal())?;
    if bytes.len() > MAX_JSON_RESPONSE_BYTES {
        return Err(ApiError::limit());
    }
    let mut response = Response::new(Body::from(bytes));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn session_cookie(token: &str, ttl_seconds: i32) -> String {
    format!(
        "{SESSION_COOKIE}={token}; Max-Age={ttl_seconds}; Path=/; Secure; HttpOnly; SameSite=Strict"
    )
}

fn guard_response(response: Response, permit: OwnedSemaphorePermit) -> Response {
    let (parts, body) = response.into_parts();
    Response::from_parts(
        parts,
        Body::new(ResponseGuard {
            inner: body,
            pending: None,
            _permit: permit,
        }),
    )
}

impl HttpBody for ResponseGuard {
    type Data = <Body as HttpBody>::Data;
    type Error = <Body as HttpBody>::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<std::result::Result<http_body::Frame<Self::Data>, Self::Error>>> {
        loop {
            if let Some(pending) = self.pending.as_mut() {
                if !pending.is_empty() {
                    let chunk_len = pending.len().min(RESPONSE_CHUNK_BYTES);
                    let chunk = Bytes::copy_from_slice(&pending[..chunk_len]);
                    let _ = pending.split_to(chunk_len);
                    return Poll::Ready(Some(Ok(http_body::Frame::data(chunk))));
                }
                self.pending = None;
            }
            match Pin::new(&mut self.inner).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => match frame.into_data() {
                    Ok(data) if data.is_empty() => continue,
                    Ok(data) => self.pending = Some(data),
                    Err(frame) => return Poll::Ready(Some(Ok(frame))),
                },
                other => return other,
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.pending.as_ref().is_none_or(Bytes::is_empty) && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        let inner = self.inner.size_hint();
        let pending = self.pending.as_ref().map_or(0, |data| data.len() as u64);
        let mut hint = http_body::SizeHint::new();
        hint.set_lower(pending.saturating_add(inner.lower()));
        if let Some(upper) = inner.upper().and_then(|value| pending.checked_add(value)) {
            hint.set_upper(upper);
        }
        hint
    }
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str) -> Self {
        Self { status, code }
    }

    fn invalid() -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request")
    }

    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized")
    }

    fn denied() -> Self {
        Self::new(StatusCode::FORBIDDEN, "denied")
    }

    fn limit() -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "limit_exceeded")
    }

    fn unavailable() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable")
    }

    fn internal() -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
    }
}

impl From<TeamError> for ApiError {
    fn from(error: TeamError) -> Self {
        match error {
            TeamError::Unauthorized => Self::unauthorized(),
            TeamError::Denied => Self::denied(),
            TeamError::Conflict(_) => Self::new(StatusCode::CONFLICT, "conflict"),
            TeamError::Expired => Self::new(StatusCode::GONE, "expired"),
            TeamError::Indeterminate => Self::new(StatusCode::CONFLICT, "indeterminate"),
            TeamError::Unavailable => Self::unavailable(),
            TeamError::Database(_) => Self::internal(),
            TeamError::Storage(error) => match error {
                Error::Invalid(_) | Error::UnsupportedGitVersion { .. } | Error::Json(_) => {
                    Self::invalid()
                }
                Error::NotFound(_) => Self::new(StatusCode::NOT_FOUND, "not_found"),
                Error::MissingObject(_) => Self::new(StatusCode::NOT_FOUND, "not_found"),
                Error::SourceDenied => Self::denied(),
                Error::SourceIndeterminate => Self::new(StatusCode::CONFLICT, "indeterminate"),
                Error::Conflict(_) => Self::new(StatusCode::CONFLICT, "conflict"),
                Error::Limit(_) => Self::limit(),
                Error::Io(_) | Error::Sql(_) => Self::internal(),
            },
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response =
            (self.status, Json(json!({"error": {"code": self.code}}))).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

async fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found")
}

pub(crate) async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::team::service::validate_origin;

    /// The full team router needs a fresh PostgreSQL 18; this serves its own capabilities
    /// route and request-limit middleware through the listener that `serve` uses.
    #[tokio::test]
    async fn the_team_listener_cuts_off_trickled_request_headers() {
        let app = Router::new()
            .route("/v1/team/capabilities", get(capabilities))
            .layer(middleware::from_fn(request_limits));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let limit = std::time::Duration::from_millis(300);
        tokio::spawn(crate::http_listener::serve_with(
            listener,
            app,
            limit,
            std::future::pending(),
        ));
        let capabilities = reqwest::get(format!("http://{address}/v1/team/capabilities"))
            .await
            .unwrap();
        assert_eq!(capabilities.status(), StatusCode::OK);
        let (elapsed, response) =
            crate::http_listener::tests::trickle_headers(address, "/v1/team/capabilities").await;
        assert!(elapsed >= limit, "{elapsed:?}");
        assert!(elapsed < std::time::Duration::from_secs(3), "{elapsed:?}");
        assert!(!response.starts_with(b"HTTP/1.1 200"), "{response:?}");
    }

    #[tokio::test]
    async fn typed_source_failures_are_distinct_and_do_not_echo_object_ids() {
        for (error, status, code) in [
            (
                Error::MissingObject("do-not-disclose-object".into()),
                StatusCode::NOT_FOUND,
                "not_found",
            ),
            (Error::SourceDenied, StatusCode::FORBIDDEN, "denied"),
            (
                Error::SourceIndeterminate,
                StatusCode::CONFLICT,
                "indeterminate",
            ),
        ] {
            let response = ApiError::from(TeamError::Storage(error)).into_response();
            assert_eq!(response.status(), status);
            let bytes = axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES)
                .await
                .unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap(),
                json!({"error":{"code":code}})
            );
        }
    }

    #[tokio::test]
    async fn canceled_blocking_work_retains_its_slot_until_finished() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState {
            database: DatabaseConfig {
                socket: directory.path().to_owned(),
                port: 5432,
                database: "unused".into(),
            },
            backend: LocalBackend::open(directory.path()).unwrap(),
            origin: Arc::from("https://source.example"),
            work: Arc::new(Semaphore::new(1)),
        };
        let (entered, started) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let working = state.clone();
        let task = tokio::spawn(async move {
            blocking(&working, move || {
                entered.send(()).unwrap();
                wait.recv().unwrap();
                Ok(())
            })
            .await
        });
        started.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let rejected = blocking(&state, || Ok(())).await.err().unwrap();
        assert_eq!(rejected.status, StatusCode::SERVICE_UNAVAILABLE);
        release.send(()).unwrap();
        // Wait for the actual worker to drop its result/permit after the
        // canceled request; this is bounded and does not assume a sleep ETA.
        tokio::task::spawn_blocking(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while state.work.available_permits() == 0 {
                assert!(std::time::Instant::now() < deadline);
                std::thread::yield_now();
            }
        })
        .await
        .unwrap();
    }

    #[test]
    fn response_body_holds_slot_and_emits_bounded_chunks() {
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = semaphore.clone().try_acquire_owned().unwrap();
        let response = guard_response(
            Response::new(Body::from(vec![1_u8; RESPONSE_CHUNK_BYTES + 7])),
            permit,
        );
        let mut body = response.into_body();
        assert_eq!(semaphore.available_permits(), 0);
        let mut context = Context::from_waker(std::task::Waker::noop());
        let Poll::Ready(Some(Ok(frame))) = Pin::new(&mut body).poll_frame(&mut context) else {
            panic!("first bounded response frame required");
        };
        assert_eq!(frame.into_data().unwrap().len(), RESPONSE_CHUNK_BYTES);
        assert_eq!(semaphore.available_permits(), 0);
        drop(body);
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[test]
    fn canonical_origin_requires_https_authority_only() {
        assert!(validate_origin("https://source.example:8443").is_ok());
        assert!(validate_origin("https://localhost").is_ok());
        assert!(validate_origin("https://source.example").is_ok());
        for origin in [
            "https://SOURCE.example",
            "https://source.example:443",
            "https://source.example:65536",
            "https://source.example:",
            "https://source.example:abc",
            "https://source.example:08443",
            "https://source.example:0",
            "https://source.example.",
            "https://127.0.0.1",
            "https://127.1",
            "https://2130706433",
            "https://0x7f000001",
            "https://[::1]",
            "https://[0:0:0:0:0:0:0:1]",
            "https://xn--bcher-kva.example",
            "https://b%C3%BCcher.example",
            "https://bücher.example",
            "http://source.example",
            "https://source.example/",
            "https://user@source.example",
            "https://source.example/path",
            "https://source.example?query",
            "https://source.example#fragment",
        ] {
            assert!(validate_origin(origin).is_err(), "accepted {origin}");
        }
    }

    #[test]
    fn native_query_binds_each_view_and_rejects_ambiguous_continuations() {
        assert!(matches!(
            native_query("revision", None).unwrap(),
            NativeQuery::Revision
        ));
        assert!(
            matches!(native_query("file", Some("path=src%2Flib.rs")).unwrap(), NativeQuery::File(path) if path == "src/lib.rs")
        );
        assert!(
            matches!(native_query("files", Some("operation=files-test&limit=1&cursor=opaque")).unwrap(), NativeQuery::Files(page) if page.operation == "files-test" && page.cursor.as_deref() == Some("opaque") && page.limit == 1)
        );
        for (view, query) in [
            ("revision", "cursor=opaque"),
            ("file", "path=src%2Flib.rs&path=other"),
            ("file", "path=src%2Flib.rs&limit=1"),
            ("files", "operation=first&operation=second"),
            ("files", "operation=bad%2Fname"),
            ("files", "limit=1"),
            ("files", "operation=files&limit=1&limit=2"),
            ("files", "cursor=first&cursor=second"),
            ("files", "limit=0"),
            ("history", "limit=257"),
            ("history", "root=forged"),
            ("diff", "left=a"),
            ("relation", "left=a&right=b&path=x"),
            ("unknown", "limit=1"),
        ] {
            assert!(native_query(view, Some(query)).is_err());
        }
    }

    #[test]
    fn range_is_inclusive_bounded_and_single() {
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=5-9"));
        let parsed = range(&headers).unwrap().unwrap();
        assert_eq!(parsed.offset, 5);
        assert_eq!(parsed.length, 5);

        headers.insert(header::RANGE, HeaderValue::from_static("bytes=9-5"));
        assert!(range(&headers).is_err());
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=0-1048576"));
        assert!(range(&headers).is_err());
        headers.append(header::RANGE, HeaderValue::from_static("bytes=0-1"));
        assert!(range(&headers).is_err());
    }

    #[test]
    fn duplicate_or_ambiguous_identity_is_rejected() {
        let token = "a".repeat(64);
        let mut headers = HeaderMap::new();
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        headers.append(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
        );
        assert!(bearer(&headers).is_err());

        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_str(&format!(
                "{SESSION_COOKIE}={token}; {SESSION_COOKIE}={token}"
            ))
            .unwrap(),
        );
        assert!(session_token(&headers).is_err());
    }
}
