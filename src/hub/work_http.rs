//! Shared engineering work coordination. The authenticated token supplies the scope.
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Path, Query, State};
use axum::routing::{get, post};
use serde_json::{Value, json};

use super::HubState;
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::store::unix_now;
use super::work::{
    Briefing, BriefingAck, BriefingQuery, WorkCheckpoint, WorkCheckpointResult, WorkItem,
    WorkStart, WorkStartResult,
};

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/standup/briefing", get(briefing))
        .route("/v1/hub/standup/start", post(start))
        .route("/v1/hub/standup/checkpoint", post(checkpoint))
        .route("/v1/hub/standup/ack", post(acknowledge))
        .route("/v1/hub/standup/work/{id}", get(work))
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
            store.work_start(&principal.token_id, &request, unix_now())
        })
        .await?,
    ))
}

async fn checkpoint(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<WorkCheckpoint>,
) -> Result<Json<WorkCheckpointResult>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_checkpoint(&principal.token_id, &request, unix_now())
        })
        .await?,
    ))
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
