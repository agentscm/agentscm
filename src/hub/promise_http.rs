//! Authenticated workspace HTTP for durable promises.
use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use serde::Deserialize;

use super::http::{ApiError, JsonBody, blocking};
use super::promise::{
    DEFAULT_PROMISE_PAGE, MAX_PROMISE_PAGE, PromiseAdmissionPreflightRequestV1,
    PromiseAdmissionPreflightV1, PromiseAdoptV1, PromiseChangeCapabilitiesV1, PromiseChangeV1,
    PromiseCreateV1, PromiseHistoryPageV1, PromiseInputPageV1, PromiseInputReceiptV1,
    PromiseInputV1, PromiseInputViewV1, PromisePageV1, PromiseReceiptV1, PromiseViewV1,
    admissions_digest, current_admission,
};
use super::store::{PromiseInputScope, unix_now};
use super::workspace::{PromiseExecution, Workspace, WorkspaceActor};
use super::{HubError, HubState};

type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/workspace/promises", get(list).post(create))
        .route("/v1/hub/workspace/promises/adopt", post(adopt))
        .route(
            "/v1/hub/workspace/promises/admission-preflight",
            post(admission_preflight),
        )
        .route("/v1/hub/workspace/promises/inputs", post(input))
        .route(
            "/v1/hub/workspace/promises/inputs/proposals",
            get(proposals),
        )
        .route(
            "/v1/hub/workspace/promises/inputs/{input_id}",
            get(input_read),
        )
        .route("/v1/hub/workspace/promises/{promise}", get(read))
        .route(
            "/v1/hub/workspace/promises/{promise}/capabilities",
            get(change_capabilities),
        )
        .route("/v1/hub/workspace/promises/{promise}/reports", get(reports))
        .route("/v1/hub/workspace/promises/{promise}/history", get(history))
        .route("/v1/hub/workspace/promises/{promise}/changes", post(change))
}

fn configured(state: &HubState) -> std::result::Result<Arc<Workspace>, ApiError> {
    state.workspace.clone().ok_or_else(ApiError::not_found)
}

#[cfg(unix)]
fn execution(state: &HubState) -> Option<Arc<PromiseExecution>> {
    state.workspace_execution.clone()
}

#[cfg(not(unix))]
fn execution(_: &HubState) -> Option<Arc<PromiseExecution>> {
    None
}

fn repositories(view: &PromiseViewV1) -> Vec<String> {
    view.current
        .definition
        .repositories
        .iter()
        .map(|repository| repository.repository_id.clone())
        .collect()
}

fn promise_error(error: HubError) -> ApiError {
    match &error {
        HubError::Invalid(code) if code == "promise_not_found" => ApiError::not_found(),
        HubError::Invalid(code) if code == "promise_forbidden" => ApiError::forbidden(),
        HubError::Invalid(code)
            if code == "promise_conflict"
                || code.starts_with("promise_conflict:")
                || code == "promise_idempotency_conflict" =>
        {
            ApiError::new(StatusCode::CONFLICT, "promise_conflict")
        }
        _ => super::workspace::api_error(error),
    }
}

fn hidden_error(error: HubError) -> ApiError {
    match &error {
        HubError::Invalid(code)
            if code == "workspace_forbidden"
                || code == "promise_forbidden"
                || code == "promise_not_found" =>
        {
            ApiError::not_found()
        }
        _ => promise_error(error),
    }
}

async fn create(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    JsonBody(request): JsonBody<PromiseCreateV1>,
) -> ApiResult<PromiseReceiptV1> {
    request.validate().map_err(promise_error)?;
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    let repositories = request
        .definition
        .repositories
        .iter()
        .map(|repository| repository.repository_id.clone())
        .collect::<Vec<_>>();
    blocking(&state, move || {
        workspace.with_promise_create(&actor, &repositories, || {
            let admissions =
                workspace.promise_admissions_for(&repositories, execution.as_deref())?;
            store.promise_create(
                workspace.workspace_id(),
                actor.user_id(),
                &request,
                &admissions,
                unix_now(),
            )
        })
    })
    .await
    .map(Json)
    .map_err(promise_error)
}

async fn adopt(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    JsonBody(request): JsonBody<PromiseAdoptV1>,
) -> ApiResult<PromiseReceiptV1> {
    request.validate().map_err(promise_error)?;
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    let repositories = request
        .definition
        .repositories
        .iter()
        .map(|repository| repository.repository_id.clone())
        .collect::<Vec<_>>();
    blocking(&state, move || {
        workspace.with_promise_create(&actor, &repositories, || {
            let admissions =
                workspace.promise_admissions_for(&repositories, execution.as_deref())?;
            store.promise_adopt(
                workspace.workspace_id(),
                actor.user_id(),
                &request,
                &admissions,
                unix_now(),
            )
        })
    })
    .await
    .map(Json)
    .map_err(hidden_error)
}

async fn input(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    JsonBody(request): JsonBody<PromiseInputV1>,
) -> ApiResult<PromiseInputReceiptV1> {
    request.validate().map_err(promise_error)?;
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    blocking(&state, move || match &request {
        PromiseInputV1::Proposal { repository_id, .. } => {
            workspace.with_promise_proposal(&actor, repository_id, || {
                let current = workspace.promise_current_admissions(execution.as_deref())?;
                let admission = current_admission(&current, repository_id);
                workspace.promise_agent_forge_scope(&actor, repository_id, &admission.forge)?;
                store.promise_input(
                    workspace.workspace_id(),
                    actor.user_id(),
                    actor.token_id(),
                    &request,
                    &admission,
                    unix_now(),
                )
            })
        }
        PromiseInputV1::Report {
            promise_id,
            repository_id,
            ..
        } => {
            let current = workspace.promise_current_admissions(execution.as_deref())?;
            let view = workspace
                .with_promise_list(&actor, |permitted| {
                    store.promise_get(workspace.workspace_id(), promise_id, permitted, &current)
                })?
                .ok_or_else(|| HubError::Invalid("promise_not_found".into()))?;
            let scope = repositories(&view);
            workspace.with_promise_report(&actor, &scope, repository_id, || {
                let admission = current_admission(&current, repository_id);
                workspace.promise_agent_forge_scope(&actor, repository_id, &admission.forge)?;
                store.promise_input(
                    workspace.workspace_id(),
                    actor.user_id(),
                    actor.token_id(),
                    &request,
                    &admission,
                    unix_now(),
                )
            })
        }
    })
    .await
    .map(Json)
    .map_err(hidden_error)
}

async fn read(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path(promise): Path<String>,
) -> ApiResult<PromiseViewV1> {
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    blocking(&state, move || {
        workspace.with_promise_list(&actor, |permitted| {
            let current = workspace.promise_current_admissions(execution.as_deref())?;
            store.promise_get(workspace.workspace_id(), &promise, permitted, &current)
        })
    })
    .await
    .map_err(hidden_error)?
    .map(Json)
    .ok_or_else(ApiError::not_found)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PageQuery {
    limit: Option<usize>,
    after: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryPageQuery {
    repository_id: String,
    limit: Option<usize>,
    after: Option<String>,
}

fn page_limit(limit: Option<usize>) -> std::result::Result<usize, ApiError> {
    let limit = limit.unwrap_or(DEFAULT_PROMISE_PAGE);
    (1..=MAX_PROMISE_PAGE)
        .contains(&limit)
        .then_some(limit)
        .ok_or_else(ApiError::invalid)
}

async fn admission_preflight(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    JsonBody(request): JsonBody<PromiseAdmissionPreflightRequestV1>,
) -> ApiResult<PromiseAdmissionPreflightV1> {
    request.validate().map_err(promise_error)?;
    let workspace = configured(&state)?;
    let execution = execution(&state);
    blocking(&state, move || {
        workspace.with_promise_create(&actor, &request.repository_ids, || {
            let admissions =
                workspace.promise_admissions_for(&request.repository_ids, execution.as_deref())?;
            let digest = admissions_digest(workspace.workspace_id(), &admissions)?;
            Ok(PromiseAdmissionPreflightV1 { admissions, digest })
        })
    })
    .await
    .map(Json)
    .map_err(hidden_error)
}

async fn change_capabilities(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path(promise): Path<String>,
) -> ApiResult<PromiseChangeCapabilitiesV1> {
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    blocking(&state, move || {
        let view = workspace
            .with_promise_list(&actor, |permitted| {
                let current = workspace.promise_current_admissions(execution.as_deref())?;
                store.promise_get(workspace.workspace_id(), &promise, permitted, &current)
            })?
            .ok_or_else(|| HubError::Invalid("promise_not_found".into()))?;
        workspace.promise_change_capabilities(&actor, &view.current)
    })
    .await
    .map(Json)
    .map_err(hidden_error)
}

async fn proposals(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    query: std::result::Result<Query<RepositoryPageQuery>, QueryRejection>,
) -> ApiResult<PromiseInputPageV1> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let limit = page_limit(query.limit)?;
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    blocking(&state, move || {
        workspace.with_promise_list(&actor, |permitted| {
            let current = workspace.promise_current_admissions(execution.as_deref())?;
            store.promise_inputs(
                workspace.workspace_id(),
                PromiseInputScope::Proposal(&query.repository_id),
                permitted,
                query.after.as_deref(),
                limit,
                &current,
            )
        })
    })
    .await
    .map(Json)
    .map_err(hidden_error)
}

async fn reports(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path(promise): Path<String>,
    query: std::result::Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<PromiseInputPageV1> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let limit = page_limit(query.limit)?;
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    blocking(&state, move || {
        workspace.with_promise_list(&actor, |permitted| {
            let current = workspace.promise_current_admissions(execution.as_deref())?;
            store.promise_inputs(
                workspace.workspace_id(),
                PromiseInputScope::Report(&promise),
                permitted,
                query.after.as_deref(),
                limit,
                &current,
            )
        })
    })
    .await
    .map(Json)
    .map_err(hidden_error)
}

async fn input_read(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path(input_id): Path<String>,
) -> ApiResult<PromiseInputViewV1> {
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    blocking(&state, move || {
        workspace.with_promise_list(&actor, |permitted| {
            let current = workspace.promise_current_admissions(execution.as_deref())?;
            store.promise_input_get(workspace.workspace_id(), &input_id, permitted, &current)
        })
    })
    .await
    .map_err(hidden_error)?
    .map(Json)
    .ok_or_else(ApiError::not_found)
}

async fn list(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    query: std::result::Result<Query<PageQuery>, QueryRejection>,
) -> ApiResult<PromisePageV1> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let limit = query.limit.unwrap_or(DEFAULT_PROMISE_PAGE);
    if !(1..=MAX_PROMISE_PAGE).contains(&limit) {
        return Err(ApiError::invalid());
    }
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    blocking(&state, move || {
        workspace.with_promise_list(&actor, |permitted| {
            let current = workspace.promise_current_admissions(execution.as_deref())?;
            store.promise_list(
                workspace.workspace_id(),
                permitted,
                query.after.as_deref(),
                limit,
                &current,
            )
        })
    })
    .await
    .map(Json)
    .map_err(promise_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    limit: Option<usize>,
    before_version: Option<u64>,
}

async fn history(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path(promise): Path<String>,
    query: std::result::Result<Query<HistoryQuery>, QueryRejection>,
) -> ApiResult<PromiseHistoryPageV1> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let limit = query.limit.unwrap_or(DEFAULT_PROMISE_PAGE);
    if !(1..=MAX_PROMISE_PAGE).contains(&limit) || query.before_version == Some(0) {
        return Err(ApiError::invalid());
    }
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    blocking(&state, move || {
        workspace.with_promise_list(&actor, |permitted| {
            let current = workspace.promise_current_admissions(execution.as_deref())?;
            store.promise_history(
                workspace.workspace_id(),
                &promise,
                permitted,
                query.before_version,
                limit,
                &current,
            )
        })
    })
    .await
    .map_err(hidden_error)?
    .map(Json)
    .ok_or_else(ApiError::not_found)
}

async fn change(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path(promise): Path<String>,
    JsonBody(request): JsonBody<PromiseChangeV1>,
) -> ApiResult<PromiseReceiptV1> {
    request.validate().map_err(promise_error)?;
    let workspace = configured(&state)?;
    let store = state.store.clone();
    let execution = execution(&state);
    blocking(&state, move || {
        let current = workspace.promise_current_admissions(execution.as_deref())?;
        let view = workspace
            .with_promise_list(&actor, |permitted| {
                store.promise_get(workspace.workspace_id(), &promise, permitted, &current)
            })
            .map_err(|error| match error {
                HubError::Invalid(code) if code == "workspace_forbidden" => {
                    HubError::Invalid("promise_not_found".into())
                }
                other => other,
            })?
            .ok_or_else(|| HubError::Invalid("promise_not_found".into()))?;
        workspace.with_promise_change(
            &actor,
            &view.current,
            &request,
            || {
                store.promise_change_replay(
                    workspace.workspace_id(),
                    actor.user_id(),
                    &promise,
                    &request,
                    &current,
                    unix_now(),
                )
            },
            |authority, new_repositories| {
                let mut admissions = new_repositories
                    .iter()
                    .map(|key| current_admission(&current, key))
                    .collect::<Vec<_>>();
                admissions.sort_by(|a, b| a.repository_id.cmp(&b.repository_id));
                store.promise_change(
                    workspace.workspace_id(),
                    actor.user_id(),
                    &promise,
                    authority,
                    &request,
                    &admissions,
                    &current,
                    unix_now(),
                )
            },
        )
    })
    .await
    .map(Json)
    .map_err(promise_error)
}
