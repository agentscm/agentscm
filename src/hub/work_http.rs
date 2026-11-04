//! Shared engineering work coordination. The authenticated token supplies the scope.
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::Value;
use serde_json::json;

use super::catchup::{CatchupAck, CatchupAckResult, CatchupPage, CatchupQuery};
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::insights::{InsightPage, InsightQuery, UsageReceipt, UsageRecord, WorkInsight};
use super::store::{search_text, unix_now};
use super::work::{
    Briefing, BriefingAck, BriefingQuery, CheckpointKind, DEFAULT_WORK_SEARCH, MAX_WORK_SEARCH,
    WorkCheckpoint, WorkCheckpointResult, WorkItem, WorkOverviewQuery, WorkScopeOverlap,
    WorkSearchPage, WorkStart, WorkState,
};
use super::work_lifecycle::overview_with_operational;
use super::work_preflight::WorkPreflightResult;
use super::work_transition::{
    OperationalOverviewV1, OperationalSelect, WorkLifecycleReader, WorkOperationalV1,
    WorkTransitionActor, WorkTransitionRequest, WorkTransitionResult,
};
use super::{HubError, HubState};

#[path = "actionable_status.rs"]
pub mod actionable_status;

pub fn routes() -> Router<HubState> {
    Router::new()
        .merge(super::coordination_http::routes())
        .route("/v1/hub/standup/narrative", get(narrative))
        .route("/v1/hub/standup/catchup", get(catchup))
        .route("/v1/hub/standup/catchup/ack", post(catchup_ack))
        .route("/v1/hub/standup/briefing", get(briefing))
        .route("/v1/hub/standup/overview", get(overview))
        .route("/v1/hub/standup/status", get(actionable_status))
        .route("/v1/hub/standup/preflight", post(preflight))
        .route("/v1/hub/standup/start", post(start))
        .route("/v1/hub/standup/checkpoint", post(checkpoint))
        .route("/v1/hub/standup/work-transition", post(work_transition))
        .route("/v1/hub/standup/operational", get(operational_overview))
        .route(
            "/v1/hub/standup/work/{id}/operational",
            get(operational_one),
        )
        .route(
            "/v1/hub/standup/work/{id}/reconciliation",
            get(reconciliation),
        )
        .route("/v1/hub/standup/ack", post(acknowledge))
        .route("/v1/hub/standup/usage", post(usage))
        .route("/v1/hub/standup/insights", get(insights))
        .route("/v1/hub/standup/value-metrics", get(value_metrics))
        .route("/v1/hub/standup/work/{id}/insights", get(insight))
        .route("/v1/hub/standup/work/{id}/pr-link", get(pr_link))
        .route("/v1/hub/standup/pr-link", post(set_pr_link))
        .route("/v1/hub/standup/work/{id}/declarations", get(declarations))
        .route("/v1/hub/standup/declarations", post(set_declarations))
        .route(
            "/v1/hub/standup/work/{id}/reconciliation-v2",
            get(reconciliation_v2),
        )
        .route("/v1/hub/standup/reconciliation", post(apply_reconciliation))
        .route("/v1/hub/standup/work/{id}", get(work))
        .route("/v1/hub/standup/work", get(work_search))
}

async fn reconciliation_v2(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(id): Path<String>,
) -> Result<Json<super::work_reconciliation::ReconciliationView>, ApiError> {
    let store = state.store.clone();
    blocking(&state, move || {
        store.work_reconciliation_get(&principal.token_id, &id, unix_now())
    })
    .await?
    .map(Json)
    .ok_or_else(ApiError::not_found)
}
async fn apply_reconciliation(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<super::work_reconciliation::ReconciliationApply>,
) -> Result<Json<super::work_reconciliation::ReconciliationResult>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_reconciliation_apply(&principal.token_id, &request, unix_now())
        })
        .await?,
    ))
}

async fn declarations(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(id): Path<String>,
) -> Result<Json<super::work_reconciliation::WorkDeclarationsView>, ApiError> {
    let store = state.store.clone();
    blocking(&state, move || {
        store.work_declarations_get(&principal.token_id, &id, unix_now())
    })
    .await?
    .map(Json)
    .ok_or_else(ApiError::not_found)
}
async fn set_declarations(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<super::work_reconciliation::WorkDeclarationsUpdate>,
) -> Result<Json<super::work_reconciliation::WorkDeclarationsResult>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_declarations_set(&principal.token_id, &request, unix_now())
        })
        .await?,
    ))
}

async fn pr_link(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(id): Path<String>,
    query: std::result::Result<Query<super::work_links::WorkPrLinkQuery>, QueryRejection>,
) -> Result<Json<super::work_links::WorkPrLinkView>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    blocking(&state, move || {
        store.work_pr_link_get(&principal.token_id, &id, &query, unix_now())
    })
    .await?
    .map(Json)
    .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "work_not_found"))
}

async fn set_pr_link(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<super::work_links::WorkPrLinkUpdate>,
) -> Result<Json<super::work_links::WorkPrLinkResult>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_pr_link_set(&principal.token_id, &request, unix_now())
        })
        .await?,
    ))
}

/// Evaluates the same exact-subject, declared-scope and fixed merge-window facts as admission,
/// without creating, claiming, resuming or otherwise changing Work.
async fn preflight(
    State(state): State<HubState>,
    Reader(principal): Reader,
    flag: std::result::Result<Query<OperationalFlag>, QueryRejection>,
    JsonBody(request): JsonBody<WorkStart>,
) -> Result<Json<WorkPreflightResult>, ApiError> {
    let operational = flag.map_err(|_| ApiError::invalid())?.0.requested()?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            let now = unix_now();
            let mut result = store.work_preflight(&principal.token_id, &request, now)?;
            if operational {
                result.operational = operational_for_overlaps(
                    &*store,
                    &principal.token_id,
                    &result.scope_overlaps,
                    now,
                )?;
            }
            Ok(result)
        })
        .await?,
    ))
}

/// `?operational=1` asks for the operational projection beside a result. Without it the
/// response has no `operational` key, so a strict older client decodes it unchanged. Other query
/// parameters are ignored, as they were on these routes before this flag existed.
#[derive(serde::Deserialize)]
struct OperationalFlag {
    operational: Option<u8>,
}

impl OperationalFlag {
    fn requested(&self) -> Result<bool, ApiError> {
        match self.operational {
            None | Some(0) => Ok(false),
            Some(1) => Ok(true),
            Some(_) => Err(ApiError::invalid()),
        }
    }
}

/// The operational projection of the Work that overlaps, for a conflict or a preflight.
pub(super) fn operational_for_overlaps(
    store: &dyn super::Store,
    token: &str,
    overlaps: &[WorkScopeOverlap],
    now: i64,
) -> super::Result<Vec<WorkOperationalV1>> {
    if overlaps.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<String> = overlaps.iter().map(|o| o.item.id.clone()).collect();
    Ok(store
        .work_operational(
            &WorkLifecycleReader::Token(token),
            &OperationalSelect::Many(&ids),
            now,
        )?
        .work)
}

async fn operational_one(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(id): Path<String>,
) -> Result<Json<WorkOperationalV1>, ApiError> {
    let store = state.store.clone();
    let page = blocking(&state, move || {
        store.work_operational(
            &WorkLifecycleReader::Token(&principal.token_id),
            &OperationalSelect::One(&id),
            unix_now(),
        )
    })
    .await?;
    page.work
        .into_iter()
        .next()
        .map(Json)
        .ok_or_else(ApiError::not_found)
}

async fn reconciliation(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(id): Path<String>,
) -> Result<Json<super::work_reconcile::WorkReconciliationV1>, ApiError> {
    let store = state.store.clone();
    blocking(&state, move || {
        super::work_reconcile::read(&*store, &principal, &id, unix_now())
    })
    .await?
    .map(Json)
    .ok_or_else(ApiError::not_found)
}

async fn operational_overview(
    State(state): State<HubState>,
    Reader(principal): Reader,
) -> Result<Json<OperationalOverviewV1>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_operational(
                &WorkLifecycleReader::Token(&principal.token_id),
                &OperationalSelect::Holders,
                unix_now(),
            )
        })
        .await?,
    ))
}

/// Release, retain, finish or reopen Work as its lead, its owner's agent or, for a reopen,
/// a session that takes it. A scope overlap on reopen is a result with a conflict and no
/// receipt, like a start.
async fn work_transition(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    flag: std::result::Result<Query<OperationalFlag>, QueryRejection>,
    JsonBody(request): JsonBody<WorkTransitionRequest>,
) -> Result<Json<WorkTransitionResult>, ApiError> {
    let operational = flag.map_err(|_| ApiError::invalid())?.0.requested()?;
    let store = state.store.clone();
    let result = blocking(&state, move || {
        let now = unix_now();
        let mut result = store.work_transition(
            &WorkTransitionActor::Agent {
                token: &principal.token_id,
            },
            &request,
            now,
        )?;
        if operational && let Some(conflict) = &mut result.conflict {
            conflict.operational =
                operational_for_overlaps(&*store, &principal.token_id, &conflict.overlaps, now)?;
        }
        Ok(result)
    })
    .await?;
    super::coordination_http::CHANGED.notify_waiters();
    Ok(Json(result))
}

/// A cached agent synthesis of the bounded source window; never acknowledges review.
async fn narrative(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<super::narrative::NarrativeQuery>, QueryRejection>,
) -> Result<Json<super::narrative::NarrativeView>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    Ok(Json(
        super::narrative::read(&state, &principal, query).await?,
    ))
}

async fn catchup(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<CatchupQuery>, QueryRejection>,
) -> Result<Json<CatchupPage>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_catchup(&principal.token_id, &query, unix_now())
        })
        .await?,
    ))
}

async fn catchup_ack(
    State(state): State<HubState>,
    Reader(principal): Reader,
    JsonBody(request): JsonBody<CatchupAck>,
) -> Result<Json<CatchupAckResult>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_catchup_ack(&principal.token_id, &request, unix_now())
        })
        .await?,
    ))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkSearchQuery {
    q: Option<String>,
    limit: Option<usize>,
    cursor: Option<String>,
}

/// Finds work of the token's repository by text, finished work included, one page at a time.
async fn work_search(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<WorkSearchQuery>, QueryRejection>,
) -> Result<Json<WorkSearchPage>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let limit = query.limit.unwrap_or(DEFAULT_WORK_SEARCH);
    if !(1..=MAX_WORK_SEARCH).contains(&limit) {
        return Err(ApiError::invalid());
    }
    let search = search_text(query.q.as_deref())?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_search(
                &principal.token_id,
                search.as_deref(),
                query.cursor.as_deref(),
                limit,
                unix_now(),
            )
        })
        .await?,
    ))
}

async fn usage(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<UsageRecord>,
) -> Result<Json<UsageReceipt>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_usage(&principal.token_id, &request, unix_now())
        })
        .await?,
    ))
}

async fn value_metrics(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<super::value_metrics::ValueMetricsQuery>, QueryRejection>,
) -> Result<Json<super::value_metrics::ValueMetrics>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    if query
        .repository
        .as_ref()
        .is_some_and(|name| name != &principal.repository)
    {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.value_metrics(&principal.token_id, &query, unix_now())
        })
        .await?,
    ))
}

async fn insight(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(id): Path<String>,
) -> Result<Json<WorkInsight>, ApiError> {
    let store = state.store.clone();
    let value = blocking(&state, move || {
        store.work_insight(&principal.token_id, &id, unix_now())
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(value))
}

async fn insights(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<InsightQuery>, QueryRejection>,
) -> Result<Json<InsightPage>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_insights(&principal.token_id, &query, unix_now())
        })
        .await?,
    ))
}

async fn overview(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<OverviewParams>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(params) = query.map_err(|_| ApiError::invalid())?;
    let operational = OperationalFlag {
        operational: params.operational,
    }
    .requested()?;
    let query = WorkOverviewQuery {
        recent_since: params.recent_since,
        limit: params
            .limit
            .unwrap_or_else(|| WorkOverviewQuery::default().limit),
    };
    let store = state.store.clone();
    let token = principal.token_id.clone();
    Ok(Json(
        blocking(&state, move || {
            let now = unix_now();
            let overview = store.work_overview(&token, &query, now)?;
            if !operational {
                return Ok(serde_json::to_value(&overview).unwrap_or(Value::Null));
            }
            let held = store.work_operational(
                &WorkLifecycleReader::Token(&token),
                &OperationalSelect::Holders,
                now,
            )?;
            Ok(overview_with_operational(&overview, &held))
        })
        .await?,
    ))
}

/// The overview query plus `operational`, which `WorkOverviewQuery` does not carry because the
/// stores, hooks and MCP share it.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OverviewParams {
    recent_since: Option<i64>,
    limit: Option<usize>,
    operational: Option<u8>,
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
    flag: std::result::Result<Query<OperationalFlag>, QueryRejection>,
    JsonBody(request): JsonBody<WorkStart>,
) -> Response {
    let operational = match flag
        .map_err(|_| ApiError::invalid())
        .and_then(|f| f.0.requested())
    {
        Ok(operational) => operational,
        Err(error) => return error.into_response(),
    };
    let store = state.store.clone();
    match blocking(&state, move || {
            let now = unix_now();
            let context =
                store.work_overview(&principal.token_id, &WorkOverviewQuery::default(), now)?;
            let mut result = store.work_start(&principal.token_id, &request, now)?;
            super::coordination_http::CHANGED.notify_waiters();
            result.context = Some(context);
            if operational && let Some(conflict) = &mut result.conflict {
                conflict.operational = operational_for_overlaps(
                    &*store,
                    &principal.token_id,
                    &conflict.overlaps,
                    now,
                )?;
            }
            Ok(result)
        })
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(HubError::WorkPreflight(preflight)) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "work_preflight_required",
                "message": "Atomic admission found changed, incomplete or conflicting source facts. No new Work was claimed.",
                "preflight": preflight,
            })),
        )
            .into_response(),
        Err(error) => ApiError::from(error).into_response(),
    }
}

async fn checkpoint(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    headers: HeaderMap,
    JsonBody(request): JsonBody<WorkCheckpoint>,
) -> Result<Json<WorkCheckpointResult>, Response> {
    let store = state.store.clone();
    let completion =
        super::insights::completion_enabled(&headers) && request.kind == CheckpointKind::Finish;
    let result = blocking(&state, move || {
        let now = unix_now();
        let result = store.work_checkpoint(&principal.token_id, &request, now)?;
        if completion
            && result.item.generation == request.generation
            && result.item.state == WorkState::Finished
            && super::insights::record_completion(
                store.as_ref(),
                &principal.token_id,
                &result.item,
                &request.session,
                now,
            )
            .is_err()
        {
            eprintln!("hub: completion report failed");
        }
        Ok(result)
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

async fn actionable_status(
    State(state): State<HubState>,
    Reader(principal): Reader,
) -> Result<Json<actionable_status::ActionableStatus>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            actionable_status::read(&*store, &principal, unix_now())
        })
        .await?,
    ))
}
