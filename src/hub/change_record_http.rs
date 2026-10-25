//! Authenticated HTTP transport for the shared change-record service.

use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Json, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};

use crate::backend::{ImmutableBackend, ObjectKey, SOURCE_PROFILE_V1};
use crate::change_record::{
    ActionFinishV1, ActionMutationReceiptV1, ActionStartV1, CaptureBatchV1, CaptureTargetV1,
    ChangeRecordResponseV1, ConversationPageV1, IngestReceiptV1, MAX_BATCH_BYTES,
    MAX_SHARED_BODY_BYTES,
};
use crate::store::digest;

use super::change_record::{SharedBodyState, conversation_readable};
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::store::{Store, unix_now};
use super::{HubError, HubState, Principal};
use serde::Deserialize;

const SHARED_BODY_DOMAIN: &str = "hub-conversation";
const SHARED_BODY_INCARNATION: &str = "v1";
/// Rows handled per sweep pass; the rest wait for the next one.
const SWEEP_BATCH: usize = 100;
const SWEEP_INTERVAL: Duration = Duration::from_secs(60 * 60);

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
        .route(
            "/v1/hub/repos/{repo}/change-records/{work_id}/conversation",
            get(list_conversation),
        )
        .route(
            "/v1/hub/repos/{repo}/change-records/{work_id}/conversation/{sha256}",
            put(upload_body)
                .layer(DefaultBodyLimit::max(MAX_SHARED_BODY_BYTES as usize))
                .get(read_body)
                .delete(delete_body),
        )
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConversationQuery {
    cursor: Option<String>,
}

async fn list_conversation(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path((repo, work_id)): Path<(String, String)>,
    Query(query): Query<ConversationQuery>,
) -> Result<Json<ConversationPageV1>, ApiError> {
    scope(&principal, &repo)?;
    Ok(Json(
        conversation_page(&state, &principal, work_id, query.cursor).await?,
    ))
}

/// The listing for the Work owner and Hub admins, shared by the HTTP and MCP reads.
pub(super) async fn conversation_page(
    state: &HubState,
    principal: &Principal,
    work_id: String,
    cursor: Option<String>,
) -> Result<ConversationPageV1, ApiError> {
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let read = blocking(state, move || {
        store.shared_conversation_page(&token, &work_id, cursor.as_deref(), unix_now())
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    if !conversation_readable(
        read.caller_owns_work,
        state.login.is_admin_user(&principal.owner),
    ) {
        return Err(ApiError::forbidden());
    }
    let page = ConversationPageV1::bounded(read.entries)?;
    page.validate_encoded_size()?;
    Ok(page)
}

/// A shared body's verified bytes for the Work owner and Hub admins, shared by the HTTP and MCP
/// reads. Unknown or never uploaded is `not_found`, expired or deleted is `gone`, and an
/// object that is missing, corrupt or not the declared size is `integrity_failure` (424).
pub(super) async fn conversation_body(
    state: &HubState,
    principal: &Principal,
    work_id: String,
    sha256: String,
) -> Result<Vec<u8>, ApiError> {
    if !lower_hex_digest(&sha256) {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    let objects = state.objects.clone();
    let admin = state.login.is_admin_user(&principal.owner);
    let token = principal.token_id.clone();
    blocking(
        state,
        move || -> Result<Result<Vec<u8>, ApiError>, HubError> {
            let now = unix_now();
            let Some(access) = store.shared_body_lookup(&token, &work_id, &sha256, now)? else {
                return Ok(Err(ApiError::not_found()));
            };
            if !access.readable(admin) {
                return Ok(Err(ApiError::forbidden()));
            }
            match access.row.state_at(now) {
                SharedBodyState::Declared => return Ok(Err(ApiError::not_found())),
                SharedBodyState::Expired | SharedBodyState::Deleted => {
                    return Ok(Err(ApiError::gone()));
                }
                SharedBodyState::Available => {}
            }
            match objects.get_verified(
                &shared_body_key(&work_id, &sha256)?,
                SHARED_BODY_INCARNATION,
            ) {
                Ok(bytes) if bytes.len() as u64 == access.row.size_bytes => Ok(Ok(bytes)),
                Ok(_)
                | Err(
                    crate::Error::NotFound(_)
                    | crate::Error::MissingObject(_)
                    | crate::Error::Invalid(_),
                ) => Ok(Err(ApiError::new(
                    StatusCode::FAILED_DEPENDENCY,
                    "integrity_failure",
                ))),
                Err(error) => Err(error.into()),
            }
        },
    )
    .await?
}

async fn read_body(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path((repo, work_id, sha256)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    scope(&principal, &repo)?;
    let bytes = conversation_body(&state, &principal, work_id, sha256).await?;
    // The body is untrusted agent output: never let a browser render or sniff it.
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=\"conversation.json\""),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn scope(principal: &Principal, repo: &str) -> Result<(), ApiError> {
    if principal.repository != repo {
        return Err(ApiError::unauthorized());
    }
    Ok(())
}

fn shared_body_key(work_id: &str, sha256: &str) -> Result<ObjectKey, HubError> {
    Ok(ObjectKey::new(
        SHARED_BODY_DOMAIN,
        work_id,
        SOURCE_PROFILE_V1,
        sha256,
    )?)
}

fn lower_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Removes the object and records it. A failure leaves the row pending for the sweep.
fn physically_delete(
    store: &dyn Store,
    objects: &dyn ImmutableBackend,
    work_id: &str,
    sha256: &str,
    now: i64,
) -> Result<(), HubError> {
    let key = shared_body_key(work_id, sha256)?;
    objects.delete_incarnation(&key, SHARED_BODY_INCARNATION)?;
    store.shared_body_mark_physically_deleted(work_id, sha256, now)
}

/// A refusal the caller should see as a status, as opposed to a store failure.
type Refusal = Result<(), ApiError>;

async fn upload_body(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, work_id, sha256)): Path<(String, String, String)>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    scope(&principal, &repo)?;
    if !lower_hex_digest(&sha256) {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    let objects = state.objects.clone();
    let retention = state.shared_conversation_retention_days * 24 * 60 * 60;
    let token = principal.token_id;
    blocking(&state, move || -> Result<Refusal, HubError> {
        let now = unix_now();
        let Some(access) = store.shared_body_lookup(&token, &work_id, &sha256, now)? else {
            return Ok(Err(ApiError::not_found()));
        };
        if !access.declared_by_caller_owner {
            return Ok(Err(ApiError::forbidden()));
        }
        match access.row.state_at(now) {
            SharedBodyState::Expired | SharedBodyState::Deleted => {
                return Ok(Err(ApiError::gone()));
            }
            SharedBodyState::Available => return Ok(Ok(())),
            SharedBodyState::Declared => {}
        }
        // Refused, never truncated: the bytes must be exactly the body the event declared.
        if body.len() as u64 != access.row.size_bytes || digest(&body) != sha256 {
            return Ok(Err(ApiError::invalid()));
        }
        let key = shared_body_key(&work_id, &sha256)?;
        objects.prepare_publication(&key, SHARED_BODY_INCARNATION)?;
        objects.put_if_absent(&key, SHARED_BODY_INCARNATION, &body)?;
        match store.shared_body_mark_available(&work_id, &sha256, now, retention)? {
            Some(SharedBodyState::Available) => Ok(Ok(())),
            _ => {
                // Deleted or expired while uploading: do not leave the bytes behind.
                if let Err(error) = objects.delete_incarnation(&key, SHARED_BODY_INCARNATION) {
                    eprintln!("hub: shared body cleanup deferred to the sweep: {error}");
                }
                Ok(Err(ApiError::gone()))
            }
        }
    })
    .await??;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_body(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, work_id, sha256)): Path<(String, String, String)>,
) -> Result<StatusCode, ApiError> {
    scope(&principal, &repo)?;
    if !lower_hex_digest(&sha256) {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    let objects = state.objects.clone();
    let admin = state.login.is_admin_user(&principal.owner);
    let token = principal.token_id;
    let actor = principal.owner.id;
    blocking(&state, move || -> Result<Refusal, HubError> {
        let now = unix_now();
        let Some(access) = store.shared_body_lookup(&token, &work_id, &sha256, now)? else {
            return Ok(Err(ApiError::not_found()));
        };
        if !access.caller_owns_work && !admin {
            return Ok(Err(ApiError::forbidden()));
        }
        // The tombstone is the decision; the object removal may fail and be retried by the sweep.
        store.shared_body_tombstone(&work_id, &sha256, &actor, now)?;
        if let Err(error) = physically_delete(&*store, &*objects, &work_id, &sha256, now) {
            eprintln!("hub: shared body removal deferred to the sweep: {error}");
        }
        Ok(Ok(()))
    })
    .await??;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub expired: usize,
    pub deleted: usize,
    pub failed: usize,
}

/// One bounded retention pass at `now`: expires due bodies, then removes the objects of every
/// expired or deleted body not yet confirmed gone. A failed removal stays pending for the next
/// pass.
pub async fn sweep_shared_bodies(state: &HubState, now: i64) -> Result<SweepReport, HubError> {
    let store = state.store.clone();
    let objects = state.objects.clone();
    blocking(state, move || {
        let mut report = SweepReport {
            expired: store.shared_bodies_expire(now, SWEEP_BATCH)?.len(),
            ..SweepReport::default()
        };
        for row in store.shared_bodies_pending_physical_delete(SWEEP_BATCH)? {
            match physically_delete(&*store, &*objects, &row.work, &row.digest, now) {
                Ok(()) => report.deleted += 1,
                Err(error) => {
                    report.failed += 1;
                    eprintln!("hub: shared body removal failed, will retry: {error}");
                }
            }
        }
        Ok(report)
    })
    .await
}

/// Runs a sweep at startup and then hourly until the process exits.
pub async fn sweep_loop(state: HubState) {
    let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(error) = sweep_shared_bodies(&state, unix_now()).await {
            eprintln!("hub: shared body sweep failed: {error}");
        }
    }
}
