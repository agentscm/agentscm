use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use agentscm::model::Revision;
use agentscm::store::Engine;
use agentscm::{Error, Result};
use axum::body::{Body, Bytes};
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use http_body::Body as HttpBody;
use serde::Serialize;
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

const MAX_CONCURRENCY: usize = 256;
const MAX_QUERY_BYTES: usize = 4 * 1024;
const MAX_SOURCE_BYTES: usize = 8 * 1024 * 1024;
const MAX_DIFF_LIMIT: usize = 10_000;
const RESPONSE_CHUNK_BYTES: usize = 16 * 1024;

#[derive(Clone)]
struct AppState {
    data_dir: PathBuf,
    engine: Engine,
    work: Arc<Semaphore>,
    /// Git checkout for collision warnings, chosen by the operator at startup.
    collisions: Option<Arc<PathBuf>>,
}

/// Bounds the serialized report before it is returned.
const MAX_COLLISIONS_BYTES: usize = 4 * 1024 * 1024;
/// Bounds how long one scan holds a work slot; the report says when it was cut short.
const COLLISIONS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

#[derive(Serialize)]
struct RevisionResponse {
    id: String,
    revision: Revision,
}

struct ReadQuery {
    path: String,
}

struct DiffQuery {
    left: String,
    right: String,
    limit: usize,
}

struct ResponseGuard {
    inner: Body,
    pending: Option<Bytes>,
    _permit: OwnedSemaphorePermit,
}

pub async fn serve(
    data_dir: PathBuf,
    bind: SocketAddr,
    public: bool,
    max_concurrency: usize,
    collisions: Option<PathBuf>,
) -> Result<()> {
    if !bind.ip().is_loopback() && !public {
        return Err(Error::Invalid(
            "binding a non-loopback address requires --public".into(),
        ));
    }
    if max_concurrency == 0 || max_concurrency > MAX_CONCURRENCY {
        return Err(Error::Limit(format!(
            "max concurrency must be between 1 and {MAX_CONCURRENCY}"
        )));
    }

    // Validate and initialize the engine before reporting the listening address.
    let engine = Engine::open(&data_dir)?;
    let data_dir = std::fs::canonicalize(data_dir)?;
    let collisions = match collisions.map(std::fs::canonicalize).transpose()? {
        Some(path) if !path.is_dir() => {
            return Err(Error::Invalid(
                "collisions repository must be a directory".into(),
            ));
        }
        path => path.map(Arc::new),
    };

    let state = AppState {
        data_dir,
        engine,
        work: Arc::new(Semaphore::new(max_concurrency)),
        collisions,
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/revisions/{revision}", get(revision))
        .route("/v1/revisions/{revision}/file", get(read_file))
        .route("/v1/diff", get(diff))
        .route("/v1/workflow", get(workflow_status))
        .route("/v1/collisions", get(collisions_status))
        .route("/v1/workflow/inspection", get(workflow_inspection))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    eprintln!("listening on {}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(Error::Io)
}

async fn workflow_status(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    if raw.as_deref().is_some_and(|raw| !raw.is_empty()) {
        return read_error(
            ApiError::limit("workflow status accepts no query fields"),
            "workflow",
        );
    }
    workflow_read(state, agentscm::guided_service::ReadRequest::Status).await
}

async fn workflow_inspection(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    let limit = match workflow_limit(raw.as_deref()) {
        Ok(limit) => limit.unwrap_or(4096),
        Err(error) => return read_error(error, "workflow"),
    };
    workflow_read(
        state,
        agentscm::guided_service::ReadRequest::Inspect { limit },
    )
    .await
}

fn workflow_limit(raw: Option<&str>) -> std::result::Result<Option<usize>, ApiError> {
    let Some(raw) = raw.filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let fields = query_fields(Some(raw))?;
    if fields.len() != 1 || fields[0].0 != "limit" {
        return Err(ApiError::limit(
            "workflow inspection accepts only one limit field",
        ));
    }
    let limit: usize = fields[0]
        .1
        .parse()
        .map_err(|_| ApiError::limit("workflow inspection limit must be an integer"))?;
    if !(1..=4096).contains(&limit) {
        return Err(ApiError::limit(
            "workflow inspection limit must be between 1 and 4096",
        ));
    }
    Ok(Some(limit))
}

async fn workflow_read(
    state: AppState,
    request: agentscm::guided_service::ReadRequest,
) -> Response {
    // This path is chosen by the operator at startup, never by an HTTP parameter.
    let data_dir = state.data_dir.clone();
    let result = blocking(state, move |engine| {
        let projected = agentscm::guided_service::read(&data_dir, &engine, request)
            .and_then(|view| Ok(serde_json::to_string(&view)?));
        projected.map_err(|error| match error {
            Error::Invalid(message) => Error::Io(std::io::Error::other(format!(
                "workflow projection validation failed: {message}"
            ))),
            error => error,
        })
    })
    .await;
    match result {
        Ok((body, permit)) => guard_response(
            api_response(StatusCode::OK, body, "application/json"),
            permit,
        ),
        Err(error) => read_error(error, "workflow"),
    }
}

#[cfg(unix)]
fn collisions_report(repository: &std::path::Path) -> Result<serde_json::Value> {
    agentscm::git_collisions::collisions(&agentscm::git_collisions::Request {
        git: PathBuf::from("/usr/bin/git"),
        repository: repository.to_path_buf(),
        base: None,
        refs: vec![],
        worktrees: true,
        active_days: Some(14),
        include_merged: false,
        deadline: Some(std::time::Instant::now() + COLLISIONS_DEADLINE),
        // Headroom for the base, version and notes outside the budgeted lists.
        max_bytes: Some(MAX_COLLISIONS_BYTES - 16 * 1024),
    })
}

#[cfg(not(unix))]
fn collisions_report(_: &std::path::Path) -> Result<serde_json::Value> {
    Err(Error::Invalid("collision checks need a Unix host".into()))
}

fn read_error(error: ApiError, what: &str) -> Response {
    let message = if error.status == StatusCode::INTERNAL_SERVER_ERROR {
        let _ = writeln!(
            std::io::stderr().lock(),
            "{what} read failed: {:?}",
            error.message
        );
        format!("The {what} read failed. Inspect local service diagnostics before retrying.")
    } else {
        error.message
    };
    api_response(
        error.status,
        json!({"error":message}).to_string(),
        "application/json",
    )
}

async fn collisions_status(State(state): State<AppState>, RawQuery(raw): RawQuery) -> Response {
    collisions_read(state, raw).await
}

async fn collisions_read(state: AppState, raw: Option<String>) -> Response {
    if raw.as_deref().is_some_and(|raw| !raw.is_empty()) {
        return read_error(
            ApiError::limit("collisions accept no query fields"),
            "collisions",
        );
    }
    // The repository comes from the serve flag, never from an HTTP parameter.
    let Some(repository) = state.collisions.clone() else {
        let error = ApiError {
            status: StatusCode::NOT_FOUND,
            message: "collisions are off; start serve with --collisions-repo".into(),
        };
        return read_error(error, "collisions");
    };
    // The scan uses the shared work slot, so --max-concurrency still bounds memory.
    let result = blocking(state, move |_| {
        // Its own messages suggest CLI flags; over HTTP the operator fixes the checkout.
        let report = match collisions_report(&repository) {
            Ok(report) => report,
            Err(error) => return Ok(Err(error.to_string())),
        };
        let json = report.to_string();
        if json.len() > MAX_COLLISIONS_BYTES {
            return Ok(Err(format!(
                "report of {} bytes exceeds {MAX_COLLISIONS_BYTES}",
                json.len()
            )));
        }
        Ok(Ok(json))
    })
    .await;
    match result {
        Ok((Ok(body), permit)) => guard_response(
            api_response(StatusCode::OK, body, "application/json"),
            permit,
        ),
        Ok((Err(detail), _)) => {
            let _ = writeln!(
                std::io::stderr().lock(),
                "collisions check failed: {detail}"
            );
            let error = ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: "The collision check could not run or its report was too large. The configured checkout needs origin/HEAD or main, at most 200 in-flight branches and worktrees, and Git 2.43 or newer at /usr/bin/git; for larger reports use agentscm collisions. The service log has the detail.".into(),
            };
            read_error(error, "collisions")
        }
        Err(error) => read_error(error, "collisions"),
    }
}

fn api_response(status: StatusCode, body: String, content_type: &'static str) -> Response {
    let mut response = (status, body).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'; base-uri 'none'"),
    );
    response
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
}

async fn revision(
    State(state): State<AppState>,
    Path(revision_id): Path<String>,
) -> std::result::Result<Response, ApiError> {
    check_text(&revision_id, "revision")?;
    let id = revision_id.clone();
    let (revision, permit) =
        blocking(state, move |engine| engine.load_revision(&revision_id)).await?;
    Ok(guard_response(
        Json(RevisionResponse { id, revision }).into_response(),
        permit,
    ))
}

async fn read_file(
    State(state): State<AppState>,
    Path(revision): Path<String>,
    RawQuery(raw_query): RawQuery,
) -> std::result::Result<Response, ApiError> {
    let query = read_query(raw_query.as_deref())?;
    check_text(&revision, "revision")?;
    check_text(&query.path, "path")?;
    let ((kind, content), permit) = blocking(state, move |engine| {
        engine.read_entry(&revision, &query.path)
    })
    .await?;
    if content.len() > MAX_SOURCE_BYTES {
        return Err(ApiError::limit("source response exceeds 8388608 bytes"));
    }
    let mut response = Response::new(Body::from(content));
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response.headers_mut().insert(
        "x-agentscm-entry-kind",
        HeaderValue::from_static(kind.as_str()),
    );
    Ok(guard_response(response, permit))
}

async fn diff(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
) -> std::result::Result<Response, ApiError> {
    let query = diff_query(raw_query.as_deref())?;
    check_text(&query.left, "left")?;
    check_text(&query.right, "right")?;
    if query.limit == 0 || query.limit > MAX_DIFF_LIMIT {
        return Err(ApiError::limit("diff limit must be between 1 and 10000"));
    }
    let (result, permit) = blocking(state, move |engine| {
        engine.diff(&query.left, &query.right, query.limit)
    })
    .await?;
    Ok(guard_response(Json(result).into_response(), permit))
}

async fn blocking<T, F>(
    state: AppState,
    operation: F,
) -> std::result::Result<(T, OwnedSemaphorePermit), ApiError>
where
    T: Send + 'static,
    F: FnOnce(Engine) -> Result<T> + Send + 'static,
{
    let permit = reserve_work(&state)?;
    blocking_reserved(state, permit, operation).await
}

fn reserve_work(state: &AppState) -> std::result::Result<OwnedSemaphorePermit, ApiError> {
    state
        .work
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            message: "server is at its concurrent work limit".to_owned(),
        })
}

async fn blocking_reserved<T, F>(
    state: AppState,
    permit: OwnedSemaphorePermit,
    operation: F,
) -> std::result::Result<(T, OwnedSemaphorePermit), ApiError>
where
    T: Send + 'static,
    F: FnOnce(Engine) -> Result<T> + Send + 'static,
{
    let engine = state.engine.clone();
    tokio::task::spawn_blocking(move || run_blocking(engine, permit, operation))
        .await
        .map_err(|_| ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: "blocking operation failed".to_owned(),
        })?
        .map_err(ApiError::from)
}

fn run_blocking<T, F>(
    engine: Engine,
    permit: OwnedSemaphorePermit,
    operation: F,
) -> Result<(T, OwnedSemaphorePermit)>
where
    F: FnOnce(Engine) -> Result<T>,
{
    operation(engine).map(|result| (result, permit))
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

fn check_text(value: &str, field: &str) -> std::result::Result<(), ApiError> {
    if value.is_empty() || value.len() > MAX_QUERY_BYTES {
        return Err(ApiError::limit(format!(
            "{field} must contain between 1 and {MAX_QUERY_BYTES} bytes"
        )));
    }
    Ok(())
}

fn read_query(raw: Option<&str>) -> std::result::Result<ReadQuery, ApiError> {
    let fields = query_fields(raw)?;
    if fields.len() != 1 || fields[0].0 != "path" {
        return Err(ApiError::limit("query must contain exactly one path field"));
    }
    Ok(ReadQuery {
        path: fields[0].1.clone(),
    })
}

fn diff_query(raw: Option<&str>) -> std::result::Result<DiffQuery, ApiError> {
    let fields = query_fields(raw)?;
    let mut left = None;
    let mut right = None;
    let mut limit = None;
    for (name, value) in fields {
        match name.as_str() {
            "left" if left.is_none() => left = Some(value),
            "right" if right.is_none() => right = Some(value),
            "limit" if limit.is_none() => {
                limit = Some(
                    value
                        .parse()
                        .map_err(|_| ApiError::limit("diff limit must be an integer"))?,
                );
            }
            _ => return Err(ApiError::limit("query has an unknown or duplicate field")),
        }
    }
    Ok(DiffQuery {
        left: left.ok_or_else(|| ApiError::limit("query is missing left"))?,
        right: right.ok_or_else(|| ApiError::limit("query is missing right"))?,
        limit: limit.unwrap_or(1_000),
    })
}

fn query_fields(raw: Option<&str>) -> std::result::Result<Vec<(String, String)>, ApiError> {
    let raw = raw.ok_or_else(|| ApiError::limit("query is required"))?;
    if raw.len() > MAX_QUERY_BYTES {
        return Err(ApiError::limit(format!(
            "query exceeds {MAX_QUERY_BYTES} bytes"
        )));
    }
    raw.split('&')
        .map(|field| {
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| ApiError::limit("query field is missing ="))?;
            Ok((decode_query(name)?, decode_query(value)?))
        })
        .collect()
}

fn decode_query(input: &str) -> std::result::Result<String, ApiError> {
    let mut bytes = Vec::with_capacity(input.len());
    let input = input.as_bytes();
    let mut index = 0;
    while index < input.len() {
        match input[index] {
            b'%' if index + 2 < input.len() => {
                let high = hex(input[index + 1])?;
                let low = hex(input[index + 2])?;
                bytes.push((high << 4) | low);
                index += 3;
            }
            b'%' => return Err(ApiError::limit("query has incomplete percent encoding")),
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
    String::from_utf8(bytes).map_err(|_| ApiError::limit("query is not valid UTF-8"))
}

fn hex(byte: u8) -> std::result::Result<u8, ApiError> {
    match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err(ApiError::limit("query has invalid percent encoding")),
    }
}

async fn shutdown_signal() {
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

impl ApiError {
    fn limit(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let status = match &error {
            Error::Invalid(_) | Error::UnsupportedGitVersion { .. } | Error::Limit(_) => {
                StatusCode::BAD_REQUEST
            }
            Error::NotFound(_) | Error::MissingObject(_) => StatusCode::NOT_FOUND,
            Error::SourceDenied => StatusCode::FORBIDDEN,
            Error::SourceIndeterminate => StatusCode::SERVICE_UNAVAILABLE,
            Error::Conflict(_) => StatusCode::CONFLICT,
            Error::Io(_) | Error::Json(_) | Error::Sql(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    #[tokio::test]
    async fn workflow_reads_share_the_existing_work_limit() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState {
            data_dir: directory.path().to_path_buf(),
            engine: Engine::open(directory.path()).unwrap(),
            work: Arc::new(Semaphore::new(1)),
            collisions: None,
        };
        let permit = reserve_work(&state).unwrap();
        let response = workflow_status(State(state.clone()), RawQuery(None)).await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(state.work.available_permits(), 0);
        drop(permit);
        assert_eq!(state.work.available_permits(), 1);
        assert!(!directory.path().join("workflow.sqlite").exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn canceled_request_keeps_permit_until_blocking_work_finishes() {
        let data_dir = tempfile::tempdir().unwrap();
        let work = Arc::new(Semaphore::new(1));
        let state = AppState {
            data_dir: data_dir.path().to_path_buf(),
            engine: Engine::open(data_dir.path()).unwrap(),
            work: Arc::clone(&work),
            collisions: None,
        };
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let task = tokio::spawn(blocking(state, move |_engine| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        }));

        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(work.clone().try_acquire_owned().is_err());

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if work.clone().try_acquire_owned().is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "permit was not released when blocking work finished"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}
