//! Authenticated workspace controls over operator-registered native repositories.
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::get;
use serde::{Deserialize, Serialize};

use super::http::{ApiError, JsonBody, blocking};
use super::workspace::{WorkspaceAction, WorkspaceActor, api_error};
use super::workspace_execution::{
    CancelRequest, ExecutionService, RunSubject, StartRequest, SubjectAuthorization,
};
use super::{HubError, HubState, Result};

type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/workspace/repositories", get(repositories))
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/subject",
            get(subject),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/subjects",
            get(subjects),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/runs",
            get(runs).post(start),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/runs/{run}",
            get(run),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/runs/{run}/cancel",
            axum::routing::post(cancel),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/runs/{run}/workflow",
            get(workflow),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/approve",
            axum::routing::post(approve),
        )
        .route(
            "/v1/hub/workspace/repositories/{repository}/profiles/{profile}/integrate",
            axum::routing::post(integrate),
        )
}

async fn call<T: Serialize + Send + 'static>(
    state: &HubState,
    work: impl FnOnce(Arc<ExecutionService>) -> Result<T> + Send + 'static,
) -> ApiResult<T> {
    let execution = state.workspace_execution.clone().ok_or_else(|| {
        ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "execution_not_configured")
    })?;
    blocking(state, move || work(execution))
        .await
        .map(Json)
        .map_err(api_error)
}

async fn repositories(
    State(state): State<HubState>,
    actor: WorkspaceActor,
) -> ApiResult<super::workspace_execution::RepositoryPage> {
    call(&state, move |service| service.repositories(&actor)).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubjectQuery {
    change_id: String,
}

async fn subject(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile)): Path<(String, String)>,
    Query(query): Query<SubjectQuery>,
) -> ApiResult<serde_json::Value> {
    call(&state, move |service| Ok(serde_json::json!({"subject":service.subject(&actor,&repository,&profile,&query.change_id)?}))).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubjectPageQuery {
    limit: Option<usize>,
    after: Option<String>,
}

async fn subjects(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile)): Path<(String, String)>,
    Query(query): Query<SubjectPageQuery>,
) -> ApiResult<super::workspace_execution::SubjectPage> {
    call(&state, move |service| {
        service.subjects(
            &actor,
            &repository,
            &profile,
            query.limit.unwrap_or(20),
            query.after.as_deref(),
        )
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunPageQuery {
    limit: Option<usize>,
    before: Option<i64>,
}

async fn runs(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path(repository): Path<String>,
    Query(query): Query<RunPageQuery>,
) -> ApiResult<super::workspace_execution::RunPage> {
    call(&state, move |service| {
        service.page(&actor, &repository, query.limit.unwrap_or(20), query.before)
    })
    .await
}

async fn start(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path(repository): Path<String>,
    JsonBody(request): JsonBody<StartRequest>,
) -> ApiResult<super::workspace_execution::WorkspaceRun> {
    call(&state, move |service| {
        service.start(&actor, &repository, request)
    })
    .await
}

async fn run(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, run)): Path<(String, String)>,
) -> ApiResult<super::workspace_execution::WorkspaceRun> {
    call(&state, move |service| {
        service.get(&actor, &repository, &run)
    })
    .await
}

async fn cancel(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, run)): Path<(String, String)>,
    JsonBody(request): JsonBody<CancelRequest>,
) -> ApiResult<super::workspace_execution::WorkspaceRun> {
    call(&state, move |service| {
        service.cancel(&actor, &repository, &run, request)
    })
    .await
}

async fn workflow(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, run)): Path<(String, String)>,
) -> ApiResult<super::skill_workflow::SkillWorkflowEnvelope> {
    call(&state, move |service| {
        service.workflow(&actor, &repository, &run)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalRequest {
    request_id: String,
    expected_generation: i64,
    subject: RunSubject,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IntegrationRequest {
    request_id: String,
    expected_generation: i64,
    subject: RunSubject,
    approval_id: String,
}

fn operation_id(actor: &WorkspaceActor, kind: &str, request_id: &str) -> Result<String> {
    if request_id.is_empty()
        || request_id.len() > 48
        || !request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(HubError::Invalid("invalid operation request id".into()));
    }
    // Native identities bind the authenticated actor and retain exact subjects.
    // Membership generation is a precondition for new operations, not history.
    Ok(format!("ws-{kind}-{}-{request_id}", actor.user_id()))
}

fn native_approval(
    id: &str,
    context: &crate::context::ReviewContext,
) -> crate::Result<crate::model::Approval> {
    Ok(crate::model::Approval {
        approval_id: id.into(),
        target: context.target.clone(),
        revision: context.revision.clone(),
        context_digest: context.digest()?,
        change: Some(context.change.clone()),
        change_version: Some(context.change_version),
        policy_epoch: context.policy_epoch,
    })
}

fn approval_response(
    approval: &crate::model::Approval,
    actor: &WorkspaceActor,
) -> serde_json::Value {
    serde_json::json!({"approval_id":approval.approval_id,"revision":approval.revision,"context_digest":approval.context_digest,"actor_id":actor.user_id()})
}

async fn approve(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile)): Path<(String, String)>,
    JsonBody(request): JsonBody<ApprovalRequest>,
) -> ApiResult<serde_json::Value> {
    call(&state, move |service| {
        let approval_id = operation_id(&actor, "approve", &request.request_id)?;
        service.with_subject_authorization(
            &actor,
            &SubjectAuthorization {
                repository: &repository,
                profile_id: &profile,
                subject: &request.subject,
                action: WorkspaceAction::ApproveRevision,
                request_id: &request.request_id,
                expected_generation: request.expected_generation,
            },
            |path, context| {
                let approval = native_approval(&approval_id, context)?;
                let authority = crate::authority::Authority::open_read_only(path)?;
                Ok(authority
                    .approval_retry(&approval)?
                    .then(|| approval_response(&approval, &actor)))
            },
            |path, context| {
                let approval = native_approval(&approval_id, context)?;
                crate::authority::Authority::open(path)?.approve_idempotent(&approval)?;
                Ok(approval_response(&approval, &actor))
            },
        )
    })
    .await
}

fn native_integration(
    id: &str,
    approval_id: &str,
    context: &crate::context::ReviewContext,
) -> crate::Result<crate::model::IntegrateRequest> {
    Ok(crate::model::IntegrateRequest {
        operation_id: id.into(),
        target: context.target.clone(),
        expected_target: context.base.as_ref().map(|base| crate::model::TargetState {
            revision: base.revision.clone(),
            version: base.target_version,
        }),
        revision: context.revision.clone(),
        context_digest: context.digest()?,
        approval_id: approval_id.into(),
        change: Some(context.change.clone()),
        change_version: Some(context.change_version),
        policy_epoch: context.policy_epoch,
    })
}

async fn integrate(
    State(state): State<HubState>,
    actor: WorkspaceActor,
    Path((repository, profile)): Path<(String, String)>,
    JsonBody(request): JsonBody<IntegrationRequest>,
) -> ApiResult<crate::model::IntegrationEvent> {
    call(&state, move |service| {
        let id = operation_id(&actor, "integrate", &request.request_id)?;
        service.with_subject_authorization(
            &actor,
            &SubjectAuthorization {
                repository: &repository,
                profile_id: &profile,
                subject: &request.subject,
                action: WorkspaceAction::IntegrateRevision,
                request_id: &request.request_id,
                expected_generation: request.expected_generation,
            },
            |path, context| {
                crate::authority::Authority::open_read_only(path)?
                    .integration_retry(&native_integration(&id, &request.approval_id, context)?)
            },
            |path, context| {
                let integration = native_integration(&id, &request.approval_id, context)?;
                crate::authority::Authority::open(path)?
                    .integrate(&crate::store::Engine::open(path)?, &integration)
            },
        )
    })
    .await
}
