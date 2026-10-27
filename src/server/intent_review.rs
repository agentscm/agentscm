//! Local intent evidence stays behind the selected-checkout browser grant.

use agentscm::intent_review_host::{IntentReviewHost, IntentReviewRequest};
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};

use super::front_door::Selected;
use super::{ApiError, AppState, blocking, guard_response};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/local/intent-reviews/{work_id}", get(status))
        .route("/v1/local/intent-reviews", post(dispatch))
        .layer(DefaultBodyLimit::max(512 * 1024))
}

async fn status(
    State(state): State<AppState>,
    Extension(selected): Extension<Selected>,
    Path(work_id): Path<String>,
) -> Result<Response, ApiError> {
    let data_dir = state.data_dir.clone();
    let repository = selected.source_repository();
    let (view, permit) = blocking(state, move |_| {
        IntentReviewHost::new(selected.root, data_dir, repository)?.status(&work_id)
    })
    .await?;
    Ok(guard_response(Json(view).into_response(), permit))
}

async fn dispatch(
    State(state): State<AppState>,
    Extension(selected): Extension<Selected>,
    Json(request): Json<IntentReviewRequest>,
) -> Result<Response, ApiError> {
    let data_dir = state.data_dir.clone();
    let repository = selected.source_repository();
    let (view, permit) = blocking(state, move |_| {
        IntentReviewHost::new(selected.root, data_dir, repository)?
            .with_human_authority()
            .dispatch(request)
    })
    .await?;
    Ok(guard_response(Json(view).into_response(), permit))
}
