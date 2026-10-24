//! Authenticated HTTP transport for the shared change-record service.

use axum::Router;
use axum::extract::{DefaultBodyLimit, Json, Path, Query, State};
use axum::routing::{get, post};

use crate::change_record::{
    ActionFinishV1, ActionMutationReceiptV1, ActionStartV1, CaptureBatchV1, CaptureTargetV1,
    ChangeRecordResponseV1, IngestReceiptV1, MAX_BATCH_BYTES,
};

use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::store::unix_now;
use super::{HubState, Principal};
use serde::Deserialize;

pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/repos/{repo}/change-records/events",
            post(ingest).layer(DefaultBodyLimit::max(MAX_BATCH_BYTES)),
        )
        .route(
            "/v1/hub/repos/{repo}/change-records/actions/start",
            post(action_start),
        )
        .route(
            "/v1/hub/repos/{repo}/change-records/actions/finish",
            post(action_finish),
        )
        .route(
            "/v1/hub/repos/{repo}/change-records/current",
            get(current_target),
        )
        .route("/v1/hub/repos/{repo}/change-records/{work_id}", get(show))
}

async fn ingest(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    JsonBody(batch): JsonBody<CaptureBatchV1>,
) -> Result<Json<IngestReceiptV1>, ApiError> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.change_ingest(&principal.token_id, &batch, unix_now())
        })
        .await?,
    ))
}

async fn action_start(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    JsonBody(start): JsonBody<ActionStartV1>,
) -> Result<Json<ActionMutationReceiptV1>, ApiError> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.change_action_start(&principal.token_id, &start, unix_now())
        })
        .await?,
    ))
}

async fn action_finish(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    JsonBody(finish): JsonBody<ActionFinishV1>,
) -> Result<Json<ActionMutationReceiptV1>, ApiError> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.change_action_finish(&principal.token_id, &finish, unix_now())
        })
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentTargetQuery {
    session_id: String,
}

async fn current_target(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    Query(query): Query<CurrentTargetQuery>,
) -> Result<Json<CaptureTargetV1>, ApiError> {
    scope(&principal, &repo)?;
    if principal.read_only {
        return Err(ApiError::unauthorized());
    }
    let store = state.store.clone();
    let token = principal.token_id;
    let target = blocking(&state, move || {
        let now = unix_now();
        let session_id = store.session_name_for(&token, Some(&query.session_id), now)?;
        Ok(store
            .capture_work_for_session(&token, &session_id, now)?
            .map(|item| CaptureTargetV1 {
                version: 1,
                work_id: item.id,
                generation: item.generation,
                session_id,
            }))
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(target))
}

async fn show(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path((repo, work_id)): Path<(String, String)>,
) -> Result<Json<ChangeRecordResponseV1>, ApiError> {
    scope(&principal, &repo)?;
    let store = state.store.clone();
    let summary = blocking(&state, move || {
        store.change_record(&principal.token_id, &work_id, unix_now())
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    let response = ChangeRecordResponseV1::bounded(summary)?;
    response.validate_encoded_size()?;
    Ok(Json(response))
}

fn scope(principal: &Principal, repo: &str) -> Result<(), ApiError> {
    if principal.repository != repo {
        return Err(ApiError::unauthorized());
    }
    Ok(())
}
