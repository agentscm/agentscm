//! Same-origin, selected-checkout adapter for the local workflow library.

use agentscm::workflow_library::{
    AcceptImport, CreateWorkflow, GenerationRequest, ImportPreviewRequest, Library, MAX_BODY_BYTES,
    Publication, PublishWorkflow, PutAttachment, SimulationRequest, UpdateWorkflow,
    ValidateAttachment,
};
use axum::extract::{DefaultBodyLimit, Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;

use super::front_door::Selected;
use super::{ApiError, AppState, blocking, guard_response, reserve_work};

pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/local/skill-library", get(skills))
        .route("/v1/local/skill-imports/preview", post(preview))
        .route("/v1/local/skill-imports/accept", post(accept_import))
        .route("/v1/local/skills/{id}/archive", post(archive_skill))
        .route(
            "/v1/local/workflow-library",
            get(workflows).post(create_workflow),
        )
        .route(
            "/v1/local/workflow-library/{id}",
            get(workflow).patch(update_workflow).delete(delete_workflow),
        )
        .route(
            "/v1/local/workflow-library/{id}/archive",
            post(archive_workflow),
        )
        .route(
            "/v1/local/workflow-library/{id}/versions",
            post(publish_workflow),
        )
        .route("/v1/local/workflow-simulations", post(simulate))
        .route("/v1/local/repository-guidance", post(guidance))
        .route(
            "/v1/local/repositories/{repository}/workflow-attachments",
            get(attachments),
        )
        .route(
            "/v1/local/repositories/{repository}/workflow-attachments/{id}",
            put(put_attachment),
        )
        .route(
            "/v1/local/repositories/{repository}/workflow-attachments/{id}/validate",
            post(validate_attachment),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
}

fn selected_repository(selected: &Selected, requested: &str) -> Result<(), ApiError> {
    if selected.repository.eq_ignore_ascii_case(requested) {
        Ok(())
    } else {
        Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "the attachment repository must match the selected checkout".into(),
        })
    }
}

fn json_response<T: Serialize>(value: T) -> Response {
    Json(value).into_response()
}

async fn library<T, F>(state: AppState, operation: F) -> Result<Response, ApiError>
where
    T: Serialize + Send + 'static,
    F: FnOnce(&mut Library) -> agentscm::Result<T> + Send + 'static,
{
    let data_dir = state.data_dir.clone();
    let (value, permit) = blocking(state, move |_| {
        let mut library = Library::open(&data_dir)?;
        operation(&mut library)
    })
    .await?;
    Ok(guard_response(json_response(value), permit))
}

async fn skills(State(state): State<AppState>) -> Result<Response, ApiError> {
    library(state, |library| {
        Ok(json!({"version": 1, "skills": library.skills()?}))
    })
    .await
}

async fn preview(
    State(state): State<AppState>,
    Json(request): Json<ImportPreviewRequest>,
) -> Result<Response, ApiError> {
    let permit = reserve_work(&state)?;
    let value = agentscm::workflow_library::preview(state.data_dir.clone(), request)
        .await
        .map_err(ApiError::from)?;
    Ok(guard_response(json_response(value), permit))
}

async fn accept_import(
    State(state): State<AppState>,
    Json(request): Json<AcceptImport>,
) -> Result<Response, ApiError> {
    library(state, move |library| library.accept_import(&request)).await
}

async fn archive_skill(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<GenerationRequest>,
) -> Result<Response, ApiError> {
    library(state, move |library| {
        library.archive_skill(&id, request.expected_generation)
    })
    .await
}

async fn workflows(State(state): State<AppState>) -> Result<Response, ApiError> {
    library(state, |library| library.workflows()).await
}

async fn workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    library(state, move |library| library.workflow_record(&id)).await
}

async fn create_workflow(
    State(state): State<AppState>,
    Json(request): Json<CreateWorkflow>,
) -> Result<Response, ApiError> {
    library(state, move |library| library.create_workflow(&request)).await
}

async fn update_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateWorkflow>,
) -> Result<Response, ApiError> {
    library(state, move |library| library.update_workflow(&id, &request)).await
}

async fn delete_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<GenerationRequest>,
) -> Result<Response, ApiError> {
    library(state, move |library| {
        library.delete_workflow(&id, request.expected_generation)?;
        Ok(json!({"deleted": true}))
    })
    .await
}

async fn archive_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<GenerationRequest>,
) -> Result<Response, ApiError> {
    library(state, move |library| {
        library.archive_workflow(&id, request.expected_generation)
    })
    .await
}

async fn publish_workflow(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(request): Json<PublishWorkflow>,
) -> Result<Response, ApiError> {
    let data_dir = state.data_dir.clone();
    let (publication, permit) = blocking(state, move |_| {
        Library::open(&data_dir)?.publish(&id, &request)
    })
    .await?;
    let response = match publication {
        Publication::Published {
            version,
            diagnostics,
        } => json_response(json!({"version": version, "diagnostics": diagnostics})),
        Publication::Rejected { diagnostics } => (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "invalid_workflow",
                "message": "workflow has blocking diagnostics",
                "diagnostics": diagnostics
            })),
        )
            .into_response(),
    };
    Ok(guard_response(response, permit))
}

async fn simulate(
    State(state): State<AppState>,
    Json(request): Json<SimulationRequest>,
) -> Result<Response, ApiError> {
    library(state, move |library| library.simulate(&request)).await
}

async fn guidance(
    State(state): State<AppState>,
    Extension(selected): Extension<Selected>,
) -> Result<Response, ApiError> {
    library(state, move |library| {
        library.guidance(&selected.repository, &selected.root)
    })
    .await
}

async fn attachments(
    State(state): State<AppState>,
    Extension(selected): Extension<Selected>,
    Path(repository): Path<String>,
) -> Result<Response, ApiError> {
    selected_repository(&selected, &repository)?;
    let repository = selected.repository;
    library(state, move |library| library.attachments(&repository)).await
}

async fn put_attachment(
    State(state): State<AppState>,
    Extension(selected): Extension<Selected>,
    Path((repository, id)): Path<(String, String)>,
    Json(request): Json<PutAttachment>,
) -> Result<Response, ApiError> {
    selected_repository(&selected, &repository)?;
    let repository = selected.repository;
    library(state, move |library| {
        library.put_attachment(&repository, &id, &request)
    })
    .await
}

async fn validate_attachment(
    State(state): State<AppState>,
    Extension(selected): Extension<Selected>,
    Path((repository, id)): Path<(String, String)>,
    Json(request): Json<ValidateAttachment>,
) -> Result<Response, ApiError> {
    selected_repository(&selected, &repository)?;
    let repository = selected.repository;
    library(state, move |library| {
        library.validate_attachment(&repository, &id, &request)
    })
    .await
}
