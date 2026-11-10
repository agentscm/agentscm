//! Explicit, bounded evidence intake. Receipt means durable storage, never verification.
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::login::HumanSession;
use super::store::unix_now;
use super::validation_evidence::{
    self, ValidationEvidencePage, ValidationEvidenceReceipt, ValidationSummaryConfig,
};
use super::{HubError, HubState};
use crate::local_validation::bundle::{MAX_BUNDLE_BYTES, ValidationBundleV1};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::{get, post},
};
use serde::Deserialize;

pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/admin/repos/{repo}/validation-summary",
            post(configure),
        )
        .route(
            "/v1/hub/repos/{repo}/validation-evidence/{proposal}",
            get(history)
                .post(submit)
                .layer(DefaultBodyLimit::max(MAX_BUNDLE_BYTES)),
        )
        .route(
            "/v1/hub/repos/{repo}/validation-evidence/{proposal}/{generation}",
            get(read),
        )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    enabled: bool,
}
async fn configure(
    State(state): State<HubState>,
    human: HumanSession,
    Path(repo): Path<String>,
    JsonBody(body): JsonBody<Configure>,
) -> Result<Json<ValidationSummaryConfig>, ApiError> {
    human.admin()?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            let row = store.repository_row(&repo)?.ok_or(HubError::Unauthorized)?;
            let config = ValidationSummaryConfig {
                repository: row.id,
                connection: row.connection,
                enabled: body.enabled,
            };
            store.configure_validation_summary(&config)?;
            Ok(config)
        })
        .await?,
    ))
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    after: Option<i64>,
    limit: Option<usize>,
}
async fn history(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path((repo, proposal)): Path<(String, i64)>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<ValidationEvidencePage>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.validation_evidence_history(
                &principal.token_id,
                &repo,
                proposal,
                query.after.unwrap_or(0),
                query.limit.unwrap_or(50),
                unix_now(),
            )
        })
        .await?,
    ))
}
async fn submit(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, proposal)): Path<(String, i64)>,
    body: Bytes,
) -> Result<Json<ValidationEvidenceReceipt>, ApiError> {
    if principal.read_only {
        return Err(ApiError::unauthorized());
    }
    let store = state.store.clone();
    let objects = state.objects.clone();
    Ok(Json(
        blocking(&state, move || {
            // Current token and exact connected repository permission before parsing or object IO.
            store.validation_evidence_history(
                &principal.token_id,
                &repo,
                proposal,
                0,
                1,
                unix_now(),
            )?;
            let row = store.repository_row(&repo)?.ok_or(HubError::Unauthorized)?;
            if !store
                .validation_summary_config(&repo)?
                .is_some_and(|c| c.enabled && c.connection == row.connection)
            {
                return Err(HubError::Invalid(
                    "Hub validation summary is not enabled".into(),
                ));
            }
            let bundle = ValidationBundleV1::from_slice(&body)?;
            let durable = validation_evidence::persist_bundle(
                &*objects,
                &repo,
                &format!("{}/{}", row.host, row.full_name),
                &bundle,
            )?;
            // Transactionally rechecks token, repository binding and configuration after object IO.
            store.receive_validation_evidence(&principal.token_id, proposal, &durable, unix_now())
        })
        .await?,
    ))
}
async fn read(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path((repo, proposal, generation)): Path<(String, i64, i64)>,
) -> Result<Json<ValidationBundleV1>, ApiError> {
    if generation <= 0 {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    let objects = state.objects.clone();
    let bundle = blocking(&state, move || {
        // Exact bounded generation lookup through authorized recorded history, never a digest supplied by the caller.
        let page = store.validation_evidence_history(
            &principal.token_id,
            &repo,
            proposal,
            generation - 1,
            1,
            unix_now(),
        )?;
        match page.entries.first().filter(|r| r.generation == generation) {
            Some(receipt) => validation_evidence::read_bundle(&*objects, receipt).map(Some),
            None => Ok(None),
        }
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(bundle))
}
