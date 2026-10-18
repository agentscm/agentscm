//! Explicitly enabled, authenticated source exchange for hosted workflows.
use std::pin::Pin;
use std::sync::{Arc, LazyLock, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::extract::{Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::Response;
use axum::routing::{get, post};
use http_body::{Body as HttpBody, Frame};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::HubState;
use super::http::{ApiError, blocking};
use super::workspace::{WorkspaceActor, api_error};
use super::workspace_execution::ExecutionService;
use super::workspace_execution::source::{
    MAX_SOURCE_BUNDLE_BYTES, SourceDownloadRequest, SourceTransferRequest,
};

// Reserve before reading an authenticated upload, and retain the permit while
// the response body exists. Slow readers cannot accumulate bounded bundles.
static METADATA_IO: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(8)));
static SOURCE_IO: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));
const MAX_METADATA_BYTES: usize = 3 * 1024 * 1024;
const BODY_TIMEOUT: Duration = Duration::from_secs(60);

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/workspace/repositories/{repository}/profiles/{profile}/source/bootstrap", get(bootstrap))
        .route("/v1/hub/workspace/repositories/{repository}/profiles/{profile}/source/download", post(download))
        .route("/v1/hub/workspace/repositories/{repository}/profiles/{profile}/source/offers", post(offer))
        .route("/v1/hub/workspace/repositories/{repository}/profiles/{profile}/source/transfers/{transfer}", get(transfer).put(upload))
}

async fn reserve(
    state: &HubState,
    actor: &WorkspaceActor,
    repository: &str,
    profile: &str,
    large: bool,
) -> Result<(Arc<ExecutionService>, OwnedSemaphorePermit), ApiError> {
    let service = state.workspace_execution.clone().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "execution_not_configured")
    })?;
    let checked = service.clone();
    let actor = actor.clone();
    let repository = repository.to_owned();
    let profile = profile.to_owned();
    blocking(state, move || {
        checked.source_authorize(&actor, &repository, &profile)
    })
    .await
    .map_err(api_error)?;
    let slots: &Arc<Semaphore> = if large { &SOURCE_IO } else { &METADATA_IO };
    let permit = Arc::clone(slots)
        .try_acquire_owned()
        .map_err(|_| ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "busy"))?;
    Ok((service, permit))
}

async fn body(request: Request, limit: usize) -> Result<Vec<u8>, ApiError> {
    if request
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .is_some_and(|size| size > limit as u64)
    {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "source_body_too_large",
        ));
    }
    tokio::time::timeout(BODY_TIMEOUT, to_bytes(request.into_body(), limit))
        .await
        .map_err(|_| ApiError::new(StatusCode::REQUEST_TIMEOUT, "source_body_timeout"))?
        .map(|value| value.to_vec())
        .map_err(|_| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "source_body_invalid"))
}

async fn metadata<T: DeserializeOwned>(request: Request) -> Result<T, ApiError> {
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        != Some("application/json")
    {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected_json",
        ));
    }
    serde_json::from_slice(&body(request, MAX_METADATA_BYTES).await?)
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid_request"))
}

struct ResponseBuffer {
    bytes: Vec<u8>,
    offset: usize,
    expired: bool,
    permit: Option<OwnedSemaphorePermit>,
}

struct TimedBody {
    shared: Arc<Mutex<ResponseBuffer>>,
    expiry: tokio::task::JoinHandle<()>,
    finished: bool,
}

impl TimedBody {
    fn new(bytes: Vec<u8>, permit: OwnedSemaphorePermit, lifetime: Duration) -> Self {
        let shared = Arc::new(Mutex::new(ResponseBuffer {
            bytes,
            offset: 0,
            expired: false,
            permit: Some(permit),
        }));
        let timer = shared.clone();
        let expiry = tokio::spawn(async move {
            tokio::time::sleep(lifetime).await;
            if let Ok(mut state) = timer.lock() {
                state.expired = true;
                state.bytes = Vec::new();
                state.permit.take();
            }
        });
        Self {
            shared,
            expiry,
            finished: false,
        }
    }
}

impl HttpBody for TimedBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.finished {
            return Poll::Ready(None);
        }
        let shared = self.shared.clone();
        let Ok(mut state) = shared.lock() else {
            self.finished = true;
            return Poll::Ready(Some(Err(std::io::Error::other(
                "source response unavailable",
            ))));
        };
        if state.expired {
            self.finished = true;
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "source response expired",
            ))));
        }
        if state.offset == state.bytes.len() {
            state.bytes = Vec::new();
            state.permit.take();
            self.finished = true;
            return Poll::Ready(None);
        }
        let end = state
            .offset
            .saturating_add(16 * 1024)
            .min(state.bytes.len());
        // Copy a small frame: a stalled transport cannot keep the full source
        // allocation alive through a shared Bytes slice after expiry.
        let chunk = Bytes::copy_from_slice(&state.bytes[state.offset..end]);
        state.offset = end;
        Poll::Ready(Some(Ok(Frame::data(chunk))))
    }
}

impl Drop for TimedBody {
    fn drop(&mut self) {
        self.expiry.abort();
        if let Ok(mut state) = self.shared.lock() {
            state.bytes = Vec::new();
            state.permit.take();
        }
    }
}

fn guarded(bytes: Vec<u8>, content_type: &'static str, permit: OwnedSemaphorePermit) -> Response {
    let mut response = Response::new(Body::new(TimedBody::new(bytes, permit, BODY_TIMEOUT)));
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert(header::X_CONTENT_TYPE_OPTIONS, "nosniff".parse().unwrap());
    response
}

fn json<T: Serialize>(value: T, permit: OwnedSemaphorePermit) -> Result<Response, ApiError> {
    let bytes = serde_json::to_vec(&value)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "source_response_invalid"))?;
    Ok(guarded(bytes, "application/json", permit))
}

async fn bootstrap(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let (service, permit) = reserve(&state, &actor, &repository, &profile, false).await?;
    let value = blocking(&state, move || {
        service.source_bootstrap(&actor, &repository, &profile)
    })
    .await
    .map_err(api_error)?;
    json(value, permit)
}

async fn download(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let (service, permit) = reserve(&state, &actor, &repository, &profile, true).await?;
    let request: SourceDownloadRequest = metadata(request).await?;
    let value = blocking(&state, move || {
        service.source_download(&actor, &repository, &profile, request)
    })
    .await
    .map_err(api_error)?;
    Ok(guarded(value, "application/octet-stream", permit))
}

async fn offer(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile)): Path<(String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let (service, permit) = reserve(&state, &actor, &repository, &profile, false).await?;
    let request: SourceTransferRequest = metadata(request).await?;
    let value = blocking(&state, move || {
        service.source_offer(&actor, &repository, &profile, request)
    })
    .await
    .map_err(api_error)?;
    json(value, permit)
}

async fn upload(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile, transfer)): Path<(String, String, String)>,
    request: Request,
) -> Result<Response, ApiError> {
    let (service, permit) = reserve(&state, &actor, &repository, &profile, true).await?;
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        != Some("application/octet-stream")
    {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected_source_bundle",
        ));
    }
    let bytes = body(request, MAX_SOURCE_BUNDLE_BYTES).await?;
    let value = blocking(&state, move || {
        service.source_upload(&actor, &repository, &profile, &transfer, &bytes)
    })
    .await
    .map_err(api_error)?;
    json(value, permit)
}

async fn transfer(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile, transfer)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    let (service, permit) = reserve(&state, &actor, &repository, &profile, false).await?;
    let value = blocking(&state, move || {
        service.source_transfer(&actor, &repository, &profile, &transfer)
    })
    .await
    .map_err(api_error)?;
    json(value, permit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unpolled_response_expires_without_blocking_metadata() {
        let source = Arc::new(Semaphore::new(1));
        let metadata = Arc::new(Semaphore::new(1));
        let body = TimedBody::new(
            vec![1; 128 * 1024],
            source.clone().acquire_owned().await.unwrap(),
            Duration::from_millis(20),
        );
        assert!(source.clone().try_acquire_owned().is_err());
        assert!(metadata.try_acquire_owned().is_ok());
        let released = tokio::time::timeout(Duration::from_secs(1), source.acquire_owned())
            .await
            .expect("unpolled source body must release its slot")
            .unwrap();
        drop(released);
        assert!(body.shared.lock().unwrap().bytes.is_empty());
        assert!(to_bytes(Body::new(body), 1024 * 1024).await.is_err());
    }
}
