//! Authenticated HTTP transport for the shared change-record service.

use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Json, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};

use crate::backend::{ImmutableBackend, ObjectKey, SOURCE_PROFILE_V1};
use crate::change_record::{
    ActionFinishV1, ActionMutationReceiptV1, ActionStartV1, CaptureBatchV1, CaptureTargetV1,
    ChangeRecordResponseV1, ConversationMissingRequestV1, ConversationMissingV1,
    ConversationPageV1, IngestReceiptV1, MAX_BATCH_BYTES, MAX_CONVERSATION_MISSING_DIGESTS,
    MAX_SHARED_BODY_BYTES,
};
use crate::store::digest;

use super::change_record::{SharedBodyRow, SharedBodyState};
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::store::{Store, unix_now};
use super::{HubError, HubState, Principal};
use serde::Deserialize;

const SHARED_BODY_DOMAIN: &str = "hub-conversation";
const SHARED_BODY_INCARNATION: &str = "v1";
/// Rows handled per sweep batch; a pass repeats batches until one comes back short.
const SWEEP_BATCH: usize = 100;
/// A pass stops starting new batches after this long; the rest waits for the next pass.
const SWEEP_BUDGET: Duration = Duration::from_secs(30);
const SWEEP_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// A pass refused with `Busy` is retried this often, this many times, before waiting for the
/// next interval.
const SWEEP_BUSY_RETRY: Duration = Duration::from_secs(5);
const SWEEP_BUSY_ATTEMPTS: usize = 12;
/// A failed object removal is retried after one hour, doubling to at most a day.
const DELETE_BACKOFF_FIRST: i64 = 60 * 60;
const DELETE_BACKOFF_MAX: i64 = 24 * 60 * 60;

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
            "/v1/hub/repos/{repo}/change-records/{work_id}/conversation/missing",
            post(missing_bodies),
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

/// The caller's own entries, or every author's for a Hub admin, shared by the HTTP and MCP reads.
pub(super) async fn conversation_page(
    state: &HubState,
    principal: &Principal,
    work_id: String,
    cursor: Option<String>,
) -> Result<ConversationPageV1, ApiError> {
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let admin = state.login.is_admin_user(&principal.owner);
    let entries = blocking(state, move || {
        store.shared_conversation_page(&token, &work_id, cursor.as_deref(), admin, unix_now())
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    let page = ConversationPageV1::bounded(entries)?;
    page.validate_encoded_size()?;
    Ok(page)
}

/// What a caller with no access to a digest is told. A caller who is not an admin gets the same
/// refusal for a digest that exists and one that does not, so the route is no existence oracle.
fn no_access(admin: bool) -> ApiError {
    if admin {
        ApiError::not_found()
    } else {
        ApiError::forbidden()
    }
}

/// A shared body's verified bytes for its author and Hub admins, shared by the HTTP and MCP
/// reads. Never uploaded is `not_found`, expired or deleted is `gone`, and an object that is
/// missing, corrupt or not the declared size is `integrity_failure` (424).
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
            let Some(row) = access.accessible(admin, now) else {
                return Ok(Err(no_access(admin)));
            };
            let row = row.clone();
            match row.state_at(now) {
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
                Ok(bytes) if bytes.len() as u64 == row.size_bytes => Ok(Ok(bytes)),
                result @ (Ok(_)
                | Err(
                    crate::Error::NotFound(_)
                    | crate::Error::MissingObject(_)
                    | crate::Error::Invalid(_),
                )) => {
                    // A vanished object is not coming back by itself: demote the rows so the
                    // authors' preflight lists the body missing and the clients upload it again.
                    if matches!(
                        result,
                        Err(crate::Error::NotFound(_) | crate::Error::MissingObject(_))
                    ) && let Err(error) =
                        store.shared_body_demote_available(&work_id, &sha256, None)
                    {
                        eprintln!("hub: shared body not demoted: {error}");
                    }
                    Ok(Err(ApiError::new(
                        StatusCode::FAILED_DEPENDENCY,
                        "integrity_failure",
                    )))
                }
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

/// Retry delay after the `attempts`-th failed removal of an object.
fn delete_backoff(attempts: i64) -> i64 {
    (DELETE_BACKOFF_FIRST << attempts.clamp(0, 10)).min(DELETE_BACKOFF_MAX)
}

/// Removes the object unless another declaration of the digest may still need it, and records
/// `row` as removed. A failure leaves the row pending for the sweep, after the backoff or at
/// once with `retry_now`.
fn physically_delete(
    store: &dyn Store,
    objects: &dyn ImmutableBackend,
    row: &SharedBodyRow,
    now: i64,
    retry_now: bool,
) -> Result<(), HubError> {
    let removed = (|| {
        if !store.shared_body_object_in_use(&row.work, &row.digest)? {
            let key = shared_body_key(&row.work, &row.digest)?;
            objects.delete_incarnation(&key, SHARED_BODY_INCARNATION)?;
            // An upload may have marked its row available while the object was going away; the
            // bytes are not here to restore, so its author uploads again.
            if store.shared_body_object_in_use(&row.work, &row.digest)? {
                store.shared_body_demote_available(&row.work, &row.digest, None)?;
            }
        }
        store.shared_body_mark_physically_deleted(&row.work, &row.digest, &row.author, now)
    })();
    if removed.is_err() {
        let retry_at = if retry_now {
            now
        } else {
            now.saturating_add(delete_backoff(row.delete_attempts))
        };
        if let Err(error) =
            store.shared_body_delete_failed(&row.work, &row.digest, &row.author, retry_at)
        {
            eprintln!("hub: shared body retry not recorded: {error}");
        }
    }
    removed
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
    let admin = state.login.is_admin_user(&principal.owner);
    blocking(&state, move || -> Result<Refusal, HubError> {
        let now = unix_now();
        let Some(access) = store.shared_body_lookup(&token, &work_id, &sha256, now)? else {
            return Ok(Err(ApiError::not_found()));
        };
        // Only the author uploads, and only against their own declaration.
        let Some(row) = access.own() else {
            return Ok(Err(no_access(admin)));
        };
        match row.state_at(now) {
            SharedBodyState::Expired | SharedBodyState::Deleted => {
                return Ok(Err(ApiError::gone()));
            }
            SharedBodyState::Available => return Ok(Ok(())),
            SharedBodyState::Declared => {}
        }
        // Refused, never truncated: the bytes must be exactly the body the event declared.
        if body.len() as u64 != row.size_bytes || digest(&body) != sha256 {
            return Ok(Err(ApiError::invalid()));
        }
        let key = shared_body_key(&work_id, &sha256)?;
        objects.prepare_publication(&key, SHARED_BODY_INCARNATION)?;
        // Available first, object second: a removal that sees this row available keeps the object
        // or demotes the row, and the put restores bytes a racing removal took.
        if store.shared_body_mark_available(&work_id, &sha256, &row.author, now, retention)?
            != Some(SharedBodyState::Available)
        {
            // Deleted or expired while uploading.
            return Ok(Err(ApiError::gone()));
        }
        // Another author's upload of the same body may already be there; the bytes are the same.
        if let Err(error) = objects.put_if_absent(&key, SHARED_BODY_INCARNATION, &body) {
            // Never leave an available row without its object: let the client retry.
            if let Err(revert) =
                store.shared_body_demote_available(&work_id, &sha256, Some(&row.author))
            {
                eprintln!("hub: shared body not reverted: {revert}");
            }
            return Err(error.into());
        }
        // Deleted between the mark and the write: the remover found no object to take, so this
        // write must not leave the bytes behind.
        if !store.shared_body_object_in_use(&work_id, &sha256)? {
            if let Err(error) = physically_delete(&*store, &*objects, row, now, true) {
                eprintln!("hub: shared body cleanup deferred to the sweep: {error}");
            }
            return Ok(Err(ApiError::gone()));
        }
        Ok(Ok(()))
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
        let Some(row) = access.accessible(admin, now) else {
            return Ok(Err(no_access(admin)));
        };
        // An author deletes their own declaration; an admin deletes every author's.
        let author = (!admin).then_some(row.author.as_str());
        // The tombstone is the decision; the object removal may fail and be retried by the sweep.
        for row in store.shared_body_tombstone(&work_id, &sha256, author, &actor, now)? {
            if row.physical_deleted_at.is_some() {
                continue;
            }
            if let Err(error) = physically_delete(&*store, &*objects, &row, now, false) {
                eprintln!("hub: shared body removal deferred to the sweep: {error}");
            }
        }
        Ok(Ok(()))
    })
    .await??;
    Ok(StatusCode::NO_CONTENT)
}

/// Which of the caller's declared bodies still need their bytes. `missing` are declared and not
/// uploaded; `gone` are not declared by the caller, expired or deleted, so the client can stop
/// offering them. Bodies already available are in neither list.
async fn missing_bodies(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, work_id)): Path<(String, String)>,
    JsonBody(request): JsonBody<ConversationMissingRequestV1>,
) -> Result<Json<ConversationMissingV1>, ApiError> {
    scope(&principal, &repo)?;
    if request.digests.len() > MAX_CONVERSATION_MISSING_DIGESTS
        || !request
            .digests
            .iter()
            .all(|digest| lower_hex_digest(digest))
    {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    let token = principal.token_id;
    let digests = request.digests;
    let rows = {
        let digests = digests.clone();
        blocking(&state, move || {
            store.shared_bodies_own(&token, &work_id, &digests, unix_now())
        })
        .await?
        .ok_or_else(ApiError::not_found)?
    };
    let now = unix_now();
    let mut answer = ConversationMissingV1::default();
    for digest in digests {
        if answer.missing.contains(&digest) || answer.gone.contains(&digest) {
            continue;
        }
        match rows
            .iter()
            .find(|row| row.digest == digest)
            .map(|row| row.state_at(now))
        {
            Some(SharedBodyState::Declared) => answer.missing.push(digest),
            Some(SharedBodyState::Available) => {}
            Some(SharedBodyState::Expired | SharedBodyState::Deleted) | None => {
                answer.gone.push(digest)
            }
        }
    }
    Ok(Json(answer))
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    pub expired: usize,
    pub deleted: usize,
    pub failed: usize,
}

/// One retention pass at `now`: expires due bodies, then removes the objects of every expired or
/// deleted body not yet confirmed gone, oldest first, in batches until one comes back short or
/// `SWEEP_BUDGET` is spent. A failed removal waits out its backoff, so it cannot starve the rest.
pub async fn sweep_shared_bodies(state: &HubState, now: i64) -> Result<SweepReport, HubError> {
    let deadline = Instant::now() + SWEEP_BUDGET;
    let retention = state.shared_conversation_retention_days * 24 * 60 * 60;
    let mut report = SweepReport::default();
    loop {
        let store = state.store.clone();
        let expired = blocking(state, move || {
            store.shared_bodies_expire(now, SWEEP_BATCH, retention)
        })
        .await?
        .len();
        report.expired += expired;
        if expired < SWEEP_BATCH || Instant::now() >= deadline {
            break;
        }
    }
    loop {
        let store = state.store.clone();
        let objects = state.objects.clone();
        let (batch, pending) = blocking(state, move || {
            let rows = store.shared_bodies_pending_physical_delete(now, SWEEP_BATCH)?;
            let mut batch = SweepReport::default();
            for row in &rows {
                match physically_delete(&*store, &*objects, row, now, false) {
                    Ok(()) => batch.deleted += 1,
                    Err(error) => {
                        batch.failed += 1;
                        eprintln!("hub: shared body removal failed, will retry: {error}");
                    }
                }
            }
            Ok((batch, rows.len()))
        })
        .await?;
        report.deleted += batch.deleted;
        report.failed += batch.failed;
        if pending < SWEEP_BATCH || Instant::now() >= deadline {
            break;
        }
    }
    Ok(report)
}

/// Runs a sweep at startup and then every ten minutes until the process exits.
pub async fn sweep_loop(state: HubState) {
    let mut ticker = tokio::time::interval(SWEEP_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        for _ in 0..SWEEP_BUSY_ATTEMPTS {
            match sweep_shared_bodies(&state, unix_now()).await {
                Err(HubError::Busy) => tokio::time::sleep(SWEEP_BUSY_RETRY).await,
                Err(error) => {
                    eprintln!("hub: shared body sweep failed: {error}");
                    break;
                }
                Ok(_) => break,
            }
        }
    }
}
