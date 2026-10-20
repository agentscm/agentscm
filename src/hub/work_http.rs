//! Shared engineering work coordination. The authenticated token supplies the scope.
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::Value;
use serde_json::json;

use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::store::unix_now;
use super::work::{
    Briefing, BriefingAck, BriefingQuery, WorkCheckpoint, WorkCheckpointResult, WorkItem,
    WorkOverview, WorkOverviewQuery, WorkStart, WorkStartResult,
};
use super::{HubError, HubState};

pub fn routes() -> Router<HubState> {
    Router::new()
        .merge(super::coordination_http::routes())
        .route("/v1/hub/standup/briefing", get(briefing))
        .route("/v1/hub/standup/overview", get(overview))
        .route("/v1/hub/standup/start", post(start))
        .route("/v1/hub/standup/checkpoint", post(checkpoint))
        .route("/v1/hub/standup/ack", post(acknowledge))
        .route("/v1/hub/standup/work/{id}", get(work))
}

async fn overview(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<WorkOverviewQuery>, QueryRejection>,
) -> Result<Json<WorkOverview>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_overview(&principal.token_id, &query, unix_now())
        })
        .await?,
    ))
}

async fn briefing(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<BriefingQuery>, QueryRejection>,
) -> Result<Json<Briefing>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_briefing(&principal.token_id, &query, unix_now())
        })
        .await?,
    ))
}

async fn work(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(id): Path<String>,
) -> Result<Json<WorkItem>, ApiError> {
    let store = state.store.clone();
    let item = blocking(&state, move || {
        store.work_get(&principal.token_id, &id, unix_now())
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(item))
}

async fn start(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<WorkStart>,
) -> Result<Json<WorkStartResult>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            let now = unix_now();
            let context =
                store.work_overview(&principal.token_id, &WorkOverviewQuery::default(), now)?;
            let mut result = store.work_start(&principal.token_id, &request, now)?;
            super::coordination_http::CHANGED.notify_waiters();
            result.context = Some(context);
            Ok(result)
        })
        .await?,
    ))
}

async fn checkpoint(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<WorkCheckpoint>,
) -> Result<Json<WorkCheckpointResult>, Response> {
    let store = state.store.clone();
    let result = blocking(&state, move || {
        store.work_checkpoint(&principal.token_id, &request, unix_now())
    })
    .await
    .map_err(work_error)?;
    super::coordination_http::CHANGED.notify_waiters();
    Ok(Json(result))
}

fn work_error(error: HubError) -> Response {
    match error {
        HubError::WorkCoordinationRequired { overlaps, request } => {
            super::coordination_http::CHANGED.notify_waiters();
            (StatusCode::CONFLICT, Json(json!({"error": "work_coordination_required",
                "overlaps": overlaps, "coordination_request": request,
                "message": "Scope was not changed. A durable request is queued for the affected leads."}))).into_response()
        },
        HubError::WorkOverlap(overlaps) => (
            StatusCode::CONFLICT,
            Json(json!({"error": "work_scope_overlap", "overlaps": overlaps,
                "alternatives": ["Split the declared scope or join the existing work as helper or reviewer.",
                    "Arrange a deliberate handover with the current lead."]})),
        ).into_response(),
        error => ApiError::from(error).into_response(),
    }
}

async fn acknowledge(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<BriefingAck>,
) -> Result<Json<Value>, ApiError> {
    let store = state.store.clone();
    blocking(&state, move || {
        store.work_ack(&principal.token_id, &request, unix_now())
    })
    .await?;
    Ok(Json(json!({"acknowledged": true})))
}
