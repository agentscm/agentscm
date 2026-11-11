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
use super::viewer::{Shown, Viewer, shown};
use super::work::{
    Briefing, BriefingAck, BriefingQuery, CheckpointKind, DEFAULT_WORK_SEARCH, MAX_WORK_SEARCH,
    WorkCheckpoint, WorkCheckpointResult, WorkItem, WorkOverviewQuery, WorkScopeOverlap,
    WorkSearchPage, WorkStart, WorkState,
};
use super::work_intent::{self, IntentFlag};
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
        .route("/v1/hub/standup/overlap/consent", post(overlap_consent))
        .route("/v1/hub/standup/overlap/consents", get(overlap_consents))
        .route("/v1/hub/standup/overlap/preflight", post(overlap_preflight))
        .route("/v1/hub/standup/overlap/start", post(overlap_start))
        .route(
            "/v1/hub/standup/overlap/checkpoint",
            post(overlap_checkpoint),
        )
        .route(
            "/v1/hub/standup/dependency-notices",
            get(dependency_notices),
        )
        .route("/v1/hub/standup/start", post(start))
        .route("/v1/hub/standup/checkpoint", post(checkpoint))
        .route("/v1/hub/standup/work-transition", post(work_transition))
        .route("/v1/hub/standup/work-transitions", post(work_transitions))
        .route("/v1/hub/standup/recovery", post(recovery))
        .route("/v1/hub/standup/recovery/preview", post(recovery_preview))
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
        .route("/v1/hub/standup/work/{id}/intent", get(work_intent))
        .route("/v1/hub/standup/work/{id}/pr-link", get(pr_link))
        .route("/v1/hub/standup/pr-link", post(set_pr_link))
        .route("/v1/hub/standup/issue-link", post(set_issue_link))
        .route("/v1/hub/standup/work/{id}/declarations", get(declarations))
        .route("/v1/hub/standup/declarations", post(set_declarations))
        .route(
            "/v1/hub/standup/work/{id}/reconciliation-v2",
            get(reconciliation_v2),
        )
        .route("/v1/hub/standup/reconciliation", post(apply_reconciliation))
        .route("/v1/hub/standup/work/{id}", get(work))
        .route("/v1/hub/standup/work", get(work_search))
        .route("/v1/hub/standup/why", get(why))
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
        store
            .work_pr_link_get(&principal.token_id, &id, &query, unix_now())
            .map(|view| view.map(|view| shown(view, &principal.owner.id)))
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
            store
                .work_pr_link_set(&principal.token_id, &request, unix_now())
                .map(|result| shown(result, &principal.owner.id))
        })
        .await?,
    ))
}

async fn set_issue_link(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<super::work_issues::WorkIssueLinkUpdate>,
) -> Result<Json<WorkCheckpointResult>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store
                .work_issue_link_set(&principal.token_id, &request, unix_now())
                .map(|result| shown(result, &principal.owner.id))
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
    dependencies: std::result::Result<Query<DependenciesFlag>, QueryRejection>,
    intent: std::result::Result<Query<IntentFlag>, QueryRejection>,
    JsonBody(request): JsonBody<WorkStart>,
) -> Result<Json<WorkPreflightResult>, ApiError> {
    let intent = intent.map_err(|_| ApiError::invalid())?.0.requested()?;
    let operational = flag.map_err(|_| ApiError::invalid())?.0.requested()?;
    let dependencies = dependencies
        .map_err(|_| ApiError::invalid())?
        .0
        .requested()?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            let now = unix_now();
            let mut result = store.work_preflight(&principal.token_id, &request, now)?;
            if !dependencies {
                result.dependencies = None;
            }
            if intent {
                result.holder_intents = Some(work_intent::for_preflight(
                    &*store,
                    &principal.token_id,
                    &result,
                    now,
                ));
            }
            result.show(Viewer(&principal.owner.id));
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

/// New prerequisite projections are opt-in on the compatibility start/preflight routes.
#[derive(serde::Deserialize)]
struct DependenciesFlag {
    dependencies: Option<u8>,
}
impl DependenciesFlag {
    fn requested(&self) -> Result<bool, ApiError> {
        match self.dependencies {
            None | Some(0) => Ok(false),
            Some(1) => Ok(true),
            Some(_) => Err(ApiError::invalid()),
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OverlapConsentsQuery {
    request_id: String,
    session: String,
}

async fn overlap_consent(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<super::work_overlap::WorkOverlapConsentRequest>,
) -> Result<Json<super::work_overlap::WorkOverlapConsentView>, Response> {
    let store = state.store.clone();
    let viewer = principal.owner.id.clone();
    let result = blocking(&state, move || {
        store
            .work_overlap_consent(&principal.token_id, &request, unix_now())
            .map(|v| shown(v, &principal.owner.id))
    })
    .await
    .map_err(|mut e| {
        e.show(Viewer(&viewer));
        work_error(e)
    })?;
    super::coordination_http::CHANGED.notify_waiters();
    Ok(Json(result))
}

async fn overlap_consents(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<OverlapConsentsQuery>, QueryRejection>,
) -> Result<Json<Vec<super::work_overlap::WorkOverlapConsentView>>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store
                .work_overlap_consents(
                    &principal.token_id,
                    &query.request_id,
                    &query.session,
                    unix_now(),
                )
                .map(|v| shown(v, &principal.owner.id))
        })
        .await?,
    ))
}

async fn overlap_preflight(
    State(state): State<HubState>,
    Reader(principal): Reader,
    intent: std::result::Result<Query<IntentFlag>, QueryRejection>,
    JsonBody(request): JsonBody<super::work_overlap::WorkOverlapStart>,
) -> Result<Json<WorkPreflightResult>, Response> {
    let intent = intent
        .map_err(|_| ApiError::invalid())
        .and_then(|f| f.0.requested())
        .map_err(IntoResponse::into_response)?;
    let store = state.store.clone();
    let viewer = principal.owner.id.clone();
    let result = blocking(&state, move || {
        store
            .work_overlap_preflight(&principal.token_id, &request, unix_now())
            .map_err(|mut error| {
                if intent {
                    work_intent::enrich_error(&*store, &principal.token_id, &mut error, unix_now());
                }
                error
            })
            .map(|mut v| {
                if intent {
                    v.holder_intents = Some(work_intent::for_preflight(
                        &*store,
                        &principal.token_id,
                        &v,
                        unix_now(),
                    ));
                }
                shown(v, &principal.owner.id)
            })
    })
    .await
    .map_err(|mut e| {
        e.show(Viewer(&viewer));
        work_error(e)
    })?;
    Ok(Json(result))
}

async fn overlap_start(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    intent: std::result::Result<Query<IntentFlag>, QueryRejection>,
    JsonBody(request): JsonBody<super::work_overlap::WorkOverlapStart>,
) -> Result<Json<super::work::WorkStartResult>, Response> {
    let intent = intent
        .map_err(|_| ApiError::invalid())
        .and_then(|f| f.0.requested())
        .map_err(IntoResponse::into_response)?;
    let store = state.store.clone();
    let viewer = principal.owner.id.clone();
    let result = blocking(&state, move || {
        store
            .work_overlap_start(&principal.token_id, &request, unix_now())
            .map_err(|mut error| {
                if intent {
                    work_intent::enrich_error(&*store, &principal.token_id, &mut error, unix_now());
                }
                error
            })
            .map(|mut v| {
                if intent {
                    v.holder_intents = Some(work_intent::for_start(
                        &*store,
                        &principal.token_id,
                        &v,
                        unix_now(),
                    ));
                }
                shown(v, &principal.owner.id)
            })
    })
    .await
    .map_err(|mut e| {
        e.show(Viewer(&viewer));
        work_error(e)
    })?;
    super::coordination_http::CHANGED.notify_waiters();
    Ok(Json(result))
}

async fn overlap_checkpoint(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    intent: std::result::Result<Query<IntentFlag>, QueryRejection>,
    JsonBody(request): JsonBody<super::work_overlap::WorkOverlapCheckpoint>,
) -> Result<Json<WorkCheckpointResult>, Response> {
    let intent = intent
        .map_err(|_| ApiError::invalid())
        .and_then(|f| f.0.requested())
        .map_err(IntoResponse::into_response)?;
    let store = state.store.clone();
    let viewer = principal.owner.id.clone();
    let result = blocking(&state, move || {
        store
            .work_overlap_checkpoint(&principal.token_id, &request, unix_now())
            .map_err(|mut error| {
                if intent {
                    work_intent::enrich_error(&*store, &principal.token_id, &mut error, unix_now());
                }
                error
            })
            .map(|v| shown(v, &principal.owner.id))
    })
    .await
    .map_err(|mut e| {
        e.show(Viewer(&viewer));
        work_error(e)
    })?;
    super::coordination_http::CHANGED.notify_waiters();
    Ok(Json(result))
}

async fn dependency_notices(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<
        Query<super::work_dependencies::WorkDependencyNoticeQuery>,
        QueryRejection,
    >,
) -> Result<Json<super::work_dependencies::WorkDependencyNoticePage>, ApiError> {
    let query = query.map_err(|_| ApiError::invalid())?.0;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_dependency_notices(&principal.token_id, &query, unix_now())
        })
        .await?,
    ))
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

/// `?reclaimed=1` asks for the lapsed holds an admission took the scope of, in `reclaimed`.
/// Without it the response has no `reclaimed` key, so a strict older client decodes it unchanged,
/// and an older Hub ignores the parameter.
#[derive(serde::Deserialize)]
pub(super) struct ReclaimedFlag {
    reclaimed: Option<u8>,
}

impl ReclaimedFlag {
    pub(super) fn requested(
        flag: std::result::Result<Query<ReclaimedFlag>, QueryRejection>,
    ) -> Result<bool, ApiError> {
        match flag.map_err(|_| ApiError::invalid())?.0.reclaimed {
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
    reclaimed: std::result::Result<Query<ReclaimedFlag>, QueryRejection>,
    intent: std::result::Result<Query<IntentFlag>, QueryRejection>,
    JsonBody(request): JsonBody<WorkTransitionRequest>,
) -> Result<Json<WorkTransitionResult>, ApiError> {
    let intent = intent.map_err(|_| ApiError::invalid())?.0.requested()?;
    let operational = flag.map_err(|_| ApiError::invalid())?.0.requested()?;
    let reclaimed = ReclaimedFlag::requested(reclaimed)?;
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
        if !reclaimed {
            result.reclaimed.clear();
        }
        if intent {
            result.holder_intents = Some(work_intent::for_transition(
                &*store,
                &principal.token_id,
                &result,
                now,
            ));
        }
        result.show(Viewer(&principal.owner.id));
        Ok(result)
    })
    .await?;
    super::coordination_http::CHANGED.notify_waiters();
    Ok(Json(result))
}

/// What the token's owner can recover: a pure read that read-only tokens may make. Every
/// remedy it lists for a read-only token is refused as `read_only`.
async fn recovery(
    State(state): State<HubState>,
    Reader(principal): Reader,
    JsonBody(query): JsonBody<super::work_recovery::RecoveryQuery>,
) -> Result<Json<super::work_recovery::WorkRecoveryV1>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            super::work_recovery::inspect(
                &*store,
                &super::work_recovery::RecoveryCaller::Agent {
                    token: &principal.token_id,
                    session: query.session.as_deref(),
                },
                &query,
                unix_now(),
            )
        })
        .await?,
    ))
}

/// What one remedy would change and the exact call that applies it. Never writes.
async fn recovery_preview(
    State(state): State<HubState>,
    Reader(principal): Reader,
    JsonBody(request): JsonBody<super::work_recovery::RemedyPreviewRequest>,
) -> Result<Json<super::work_recovery::RemedyPreviewV1>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            super::work_recovery::preview(
                &*store,
                &super::work_recovery::RecoveryCaller::Agent {
                    token: &principal.token_id,
                    session: request.session.as_deref(),
                },
                &request,
                unix_now(),
            )
        })
        .await?,
    ))
}

/// Up to 25 transitions, each applied in its own transaction with its own outcome.
async fn work_transitions(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    intent: std::result::Result<Query<IntentFlag>, QueryRejection>,
    JsonBody(batch): JsonBody<super::work_recovery::WorkTransitionBatch>,
) -> Result<Json<super::work_recovery::WorkTransitionBatchV1>, ApiError> {
    let intent = intent.map_err(|_| ApiError::invalid())?.0.requested()?;
    let store = state.store.clone();
    let result = blocking(&state, move || {
        super::work_recovery::apply_batch(
            &*store,
            &WorkTransitionActor::Agent {
                token: &principal.token_id,
            },
            &batch,
            unix_now(),
        )
        .map(|mut result| {
            if intent {
                result.holder_intents = Some(work_intent::for_batch(
                    &*store,
                    &principal.token_id,
                    &result,
                    unix_now(),
                ));
            }
            shown(result, &principal.owner.id)
        })
    })
    .await?;
    if result.applied > 0 {
        super::coordination_http::CHANGED.notify_waiters();
    }
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
            let mut page = store.work_catchup(&principal.token_id, &query, unix_now())?;
            page.show(Viewer(&principal.owner.id));
            Ok(page)
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

/// The Work recorded for a commit or a path, with its decisions; `known: false` when none is.
async fn why(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<super::why::WhyQuery>, QueryRejection>,
) -> Result<Json<super::why::WhyAnswer>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let subject = query.subject().map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.work_why(&principal.token_id, &subject, unix_now())
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

async fn work_intent(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(id): Path<String>,
) -> Result<Json<super::work_intent::WorkIntentV1>, ApiError> {
    let store = state.store.clone();
    let value = blocking(&state, move || {
        store.work_intent(&principal.token_id, &id, unix_now())
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(value))
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
    let stages = match params.stages {
        None | Some(0) => false,
        Some(1) => true,
        Some(_) => return Err(ApiError::invalid()),
    };
    let query = WorkOverviewQuery {
        recent_since: params.recent_since,
        limit: params
            .limit
            .unwrap_or_else(|| WorkOverviewQuery::default().limit),
        after: params.after,
    };
    let store = state.store.clone();
    let token = principal.token_id.clone();
    Ok(Json(
        blocking(&state, move || {
            let now = unix_now();
            let mut overview = store.work_overview(&token, &query, now)?;
            overview.show(Viewer(&principal.owner.id));
            // Stages are opt-in: an older client decodes the overview strictly.
            let value = if stages {
                super::work_reconcile::overview_with_stages(&*store, &principal, &overview, now).0
            } else {
                serde_json::to_value(&overview).unwrap_or(Value::Null)
            };
            if !operational {
                return Ok(value);
            }
            let held = store.work_operational(
                &WorkLifecycleReader::Token(&token),
                &OperationalSelect::Holders,
                now,
            )?;
            Ok(super::work_lifecycle::attach_operational(value, &held))
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
    after: Option<String>,
    operational: Option<u8>,
    stages: Option<u8>,
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
            let mut briefing = store.work_briefing(&principal.token_id, &query, unix_now())?;
            briefing.show(Viewer(&principal.owner.id));
            Ok(briefing)
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
        let mut item = store.work_get(&principal.token_id, &id, unix_now())?;
        item.show(Viewer(&principal.owner.id));
        Ok(item)
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(item))
}

async fn start(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    flag: std::result::Result<Query<OperationalFlag>, QueryRejection>,
    reclaimed: std::result::Result<Query<ReclaimedFlag>, QueryRejection>,
    dependencies: std::result::Result<Query<DependenciesFlag>, QueryRejection>,
    intent: std::result::Result<Query<IntentFlag>, QueryRejection>,
    JsonBody(request): JsonBody<WorkStart>,
) -> Response {
    let intent = match intent
        .map_err(|_| ApiError::invalid())
        .and_then(|f| f.0.requested())
    {
        Ok(v) => v,
        Err(e) => return e.into_response(),
    };
    let dependencies = match dependencies
        .map_err(|_| ApiError::invalid())
        .and_then(|f| f.0.requested())
    {
        Ok(value) => value,
        Err(error) => return error.into_response(),
    };
    let operational = match flag
        .map_err(|_| ApiError::invalid())
        .and_then(|f| f.0.requested())
    {
        Ok(operational) => operational,
        Err(error) => return error.into_response(),
    };
    let reclaimed = match ReclaimedFlag::requested(reclaimed) {
        Ok(reclaimed) => reclaimed,
        Err(error) => return error.into_response(),
    };
    let store = state.store.clone();
    let viewer = principal.owner.id.clone();
    match blocking(&state, move || {
        let now = unix_now();
        let context =
            store.work_overview(&principal.token_id, &WorkOverviewQuery::default(), now)?;
        let mut result = store
            .work_start(&principal.token_id, &request, now)
            .map_err(|mut error| {
                if intent {
                    work_intent::enrich_error(&*store, &principal.token_id, &mut error, now);
                }
                error
            })?;
        if !dependencies && let Some(preflight) = &mut result.preflight {
            preflight.dependencies = None;
        }
        super::coordination_http::CHANGED.notify_waiters();
        result.context = Some(context);
        if !reclaimed {
            result.reclaimed.clear();
        }
        if operational && let Some(conflict) = &mut result.conflict {
            conflict.operational =
                operational_for_overlaps(&*store, &principal.token_id, &conflict.overlaps, now)?;
        }
        if intent {
            result.holder_intents = Some(work_intent::for_start(
                &*store,
                &principal.token_id,
                &result,
                now,
            ));
        }
        result.show(Viewer(&principal.owner.id));
        Ok(result)
    })
    .await
    {
        Ok(result) => Json(result).into_response(),
        Err(mut error) => {
            if !dependencies && let HubError::WorkPreflight(preflight) = &mut error {
                preflight.dependencies = None;
            }
            error.show(Viewer(&viewer));
            ApiError::from(error).into_response()
        }
    }
}

async fn checkpoint(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    headers: HeaderMap,
    reclaimed: std::result::Result<Query<ReclaimedFlag>, QueryRejection>,
    intent: std::result::Result<Query<IntentFlag>, QueryRejection>,
    JsonBody(request): JsonBody<WorkCheckpoint>,
) -> Result<Json<WorkCheckpointResult>, Response> {
    let intent = intent
        .map_err(|_| ApiError::invalid())
        .and_then(|f| f.0.requested())
        .map_err(IntoResponse::into_response)?;
    let reclaimed = ReclaimedFlag::requested(reclaimed).map_err(IntoResponse::into_response)?;
    let store = state.store.clone();
    let viewer = principal.owner.id.clone();
    let completion =
        super::insights::completion_enabled(&headers) && request.kind == CheckpointKind::Finish;
    let result = blocking(&state, move || {
        let now = unix_now();
        let mut result = store
            .work_checkpoint(&principal.token_id, &request, now)
            .map_err(|mut error| {
                if intent {
                    work_intent::enrich_error(&*store, &principal.token_id, &mut error, now);
                }
                error
            })?;
        if !reclaimed {
            result.reclaimed.clear();
        }
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
        result.show(Viewer(&principal.owner.id));
        Ok(result)
    })
    .await
    .map_err(|mut error| {
        error.show(Viewer(&viewer));
        work_error(error)
    })?;
    super::coordination_http::CHANGED.notify_waiters();
    Ok(Json(result))
}

fn work_error(error: HubError) -> Response {
    if matches!(&error, HubError::WorkCoordinationRequired { .. }) {
        super::coordination_http::CHANGED.notify_waiters();
    }
    ApiError::from(error).into_response()
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
    let setup = state.onboarding.clone();
    Ok(Json(
        blocking(&state, move || {
            let mut status = actionable_status::read(&*store, &setup, &principal, unix_now())?;
            status.show(Viewer(&principal.owner.id));
            Ok(status)
        })
        .await?,
    ))
}
