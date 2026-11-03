//! Administrator enrollment of trusted local CI devices, and the recheck of current
//! authorization when an offline result is accepted.
//!
//! The Hub holds public keys only. A device proves possession of its key by signing an
//! [`EnrollmentStatement`] that names its own id, the repository label, the owner and the
//! generation the change produces; the device chooses its `lcd_` id, and the store refuses
//! an id that exists. Nothing here accepts a computer credential, agent token or read-only
//! token for a change: every write is an administrator session. No route here submits
//! results, edits policy or publishes forge statuses.
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Json;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::HubState;
use super::connection::ConnectedRepository;
use super::http::{ApiError, JsonBody, blocking};
use super::login::HumanSession;
use super::login::devices::{Denial, Owner, repository_role};
use super::store::{
    LocalCiDevice, LocalCiDeviceWrite, LocalCiRefusal, LocalCiRequest, User, local_ci_label,
    lower_hex_of, unix_now,
};
use crate::evidence::{EvaluationClock, validate_identifier};
use crate::local_ci::{
    DeviceAuthorization, DeviceClaim, DeviceDecision, DeviceKey, DeviceRefusal, DeviceUnknown,
    EnrollmentExpectation, EnrollmentStatement, LOCAL_CI_CHECK_KIND, OwnerIdentity, OwnerStanding,
    VERSION, assess_device, verify_enrollment,
};

type ApiResult<T> = std::result::Result<Json<T>, ApiError>;

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/admin/local-ci/devices", get(list).post(enroll))
        .route("/v1/hub/admin/local-ci/devices/{id}/rotate", post(rotate))
        .route("/v1/hub/admin/local-ci/devices/{id}/revoke", post(revoke))
        .route("/v1/hub/local-ci/devices", get(own))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollBody {
    request_id: String,
    /// `host/full_name` of a connected repository.
    repository: String,
    /// Hub user id of the owner.
    owner: String,
    /// When the enrolled key stops being valid.
    not_after_ms: u64,
    statement: EnrollmentStatement,
    signature: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RotateBody {
    request_id: String,
    expected_generation: u64,
    not_after_ms: u64,
    /// For generation `expected_generation + 1`.
    statement: EnrollmentStatement,
    signature: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RevokeBody {
    request_id: String,
    expected_generation: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryQuery {
    /// `host/full_name` of a connected repository.
    repository: String,
}

fn refused(status: StatusCode, kind: &'static str, message: &str) -> ApiError {
    ApiError::new(status, kind).with_message(message)
}

fn now_ms() -> u64 {
    u64::try_from(unix_now()).unwrap_or(0).saturating_mul(1000)
}

fn owner_identity(user: &User) -> OwnerIdentity {
    OwnerIdentity {
        provider: user.identity.provider.clone(),
        host: user.identity.host.clone(),
        person_id: user.identity.user_id.to_string(),
    }
}

fn request(request_id: &str, what: &Value) -> std::result::Result<LocalCiRequest, ApiError> {
    // The store would report a bad id as unavailable; it is the caller's mistake.
    validate_identifier("request id", request_id).map_err(|_| {
        refused(
            StatusCode::BAD_REQUEST,
            "invalid_request_id",
            "the request id must be 1 to 128 letters, digits or - _ . : /",
        )
    })?;
    let bytes = serde_json::to_vec(what).map_err(|_| ApiError::invalid())?;
    Ok(LocalCiRequest {
        request_id: request_id.to_owned(),
        fingerprint: lower_hex_of(&bytes),
    })
}

/// An exact replay of an applied request answers before any check that depends on the clock,
/// the owner, the forge or the repository, so a retry after a lost response gets the original
/// answer. The same id with a different request or device conflicts.
async fn replay(
    state: &HubState,
    request: &LocalCiRequest,
    device_id: &str,
) -> std::result::Result<Option<Json<Value>>, ApiError> {
    let (store, id) = (state.store.clone(), request.request_id.clone());
    let Some((seen_device, fingerprint)) =
        blocking(state, move || store.local_ci_request(&id)).await?
    else {
        return Ok(None);
    };
    if seen_device != device_id || fingerprint != request.fingerprint {
        return Err(refusal(LocalCiRefusal::RequestConflict));
    }
    written(LocalCiDeviceWrite::Replayed(
        current(state, device_id).await?,
    ))
    .map(Some)
}

async fn find_repository(
    state: &HubState,
    label: &str,
) -> std::result::Result<Option<ConnectedRepository>, ApiError> {
    let (host, name) = label.split_once('/').ok_or_else(ApiError::invalid)?;
    let (store, host, name) = (state.store.clone(), host.to_owned(), name.to_owned());
    Ok(blocking(state, move || store.repository_named(&host, &name)).await?)
}

async fn repository_for(
    state: &HubState,
    label: &str,
) -> std::result::Result<ConnectedRepository, ApiError> {
    find_repository(state, label).await?.ok_or_else(|| {
        refused(
            StatusCode::NOT_FOUND,
            "unknown_repository",
            "that repository isn't connected to this Hub",
        )
    })
}

fn key_from(statement: &EnrollmentStatement, now: u64, not_after_ms: u64) -> DeviceKey {
    DeviceKey {
        key_id: statement.key_id.clone(),
        public_key_hex: statement.public_key_hex.clone(),
        not_before_ms: now,
        not_after_ms,
        rotated_at_ms: None,
    }
}

fn check_expiry(not_after_ms: u64, now: u64) -> std::result::Result<(), ApiError> {
    if not_after_ms > now && not_after_ms <= i64::MAX as u64 {
        return Ok(());
    }
    Err(refused(
        StatusCode::BAD_REQUEST,
        "invalid_expiry",
        "the key expiry must be in the future and fit a signed 64-bit millisecond count",
    ))
}

fn enrollment_invalid() -> ApiError {
    refused(
        StatusCode::BAD_REQUEST,
        "enrollment_invalid",
        "the enrollment statement or its signature doesn't match this repository, owner, device and generation",
    )
}

fn refusal(refusal: LocalCiRefusal) -> ApiError {
    match refusal {
        LocalCiRefusal::UnknownDevice => refused(
            StatusCode::NOT_FOUND,
            "unknown_device",
            "no such local CI device",
        ),
        LocalCiRefusal::DeviceExists => refused(
            StatusCode::CONFLICT,
            "device_exists",
            "a device with that id is already enrolled",
        ),
        LocalCiRefusal::StaleGeneration { current } => refused(
            StatusCode::CONFLICT,
            "stale_generation",
            "the device changed since you read it; read it again and retry",
        )
        .with_field("current_generation", json!(current)),
        LocalCiRefusal::RequestConflict => refused(
            StatusCode::CONFLICT,
            "request_conflict",
            "that request id was already used for a different request",
        ),
        LocalCiRefusal::DuplicateKey => refused(
            StatusCode::CONFLICT,
            "duplicate_key",
            "that key is, or once was, enrolled on a device",
        ),
    }
}

fn written(write: LocalCiDeviceWrite) -> ApiResult<Value> {
    let (device, replayed) = match write {
        LocalCiDeviceWrite::Applied(device) => (device, false),
        LocalCiDeviceWrite::Replayed(device) => (device, true),
        LocalCiDeviceWrite::Refused(why) => return Err(refusal(why)),
    };
    Ok(Json(
        json!({"device": device_json(&device), "replayed": replayed}),
    ))
}

/// Public fields only: key ids and public keys, never anything secret.
fn device_json(device: &LocalCiDevice) -> Value {
    let a = &device.authorization;
    let keys: Vec<_> = a
        .keys
        .iter()
        .map(|key| {
            json!({"key_id": key.key_id, "public_key_hex": key.public_key_hex,
                "not_before_ms": key.not_before_ms, "not_after_ms": key.not_after_ms,
                "rotated_at_ms": key.rotated_at_ms})
        })
        .collect();
    json!({"device_id": a.device_id, "repository": a.repository, "owner": device.owner,
        "generation": a.generation, "digest": device.digest, "revoked_at_ms": a.revoked_at_ms,
        "allowed_check_kinds": a.allowed_check_kinds, "keys": keys,
        "created_at": device.created_at, "updated_at": device.updated_at})
}

/// The owner's role is asked with the Hub's own connection credential, never cached; an
/// uncertain answer enrolls nothing.
fn role_error(denial: Denial) -> ApiError {
    match denial {
        Denial::Role(_) => refused(
            StatusCode::CONFLICT,
            "owner_role_missing",
            "the owner doesn't currently have a role on this repository",
        ),
        Denial::Kind(_) => refused(
            StatusCode::SERVICE_UNAVAILABLE,
            "role_check_unavailable",
            "the Hub couldn't check the owner's role right now; try again in a minute",
        ),
    }
}

async fn enroll(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<EnrollBody>,
) -> ApiResult<Value> {
    let by = human.admin()?.user.id.clone();
    let repo = repository_for(&state, &body.repository).await?;
    let label = local_ci_label(&repo.host, &repo.full_name);
    let request = request(
        &body.request_id,
        &json!({"op": "enroll", "repository": repo.id, "owner": body.owner,
            "not_after_ms": body.not_after_ms, "statement": body.statement,
            "signature": body.signature}),
    )?;
    if let Some(replayed) = replay(&state, &request, &body.statement.device_id).await? {
        return Ok(replayed);
    }
    let (store, id) = (state.store.clone(), body.owner.clone());
    let owner = blocking(&state, move || store.user_by_id(&id))
        .await?
        .ok_or_else(|| refused(StatusCode::NOT_FOUND, "unknown_owner", "no such Hub user"))?;
    if owner.disabled {
        return Err(refused(
            StatusCode::CONFLICT,
            "owner_disabled",
            "that Hub user is disabled",
        ));
    }
    let identity = owner_identity(&owner);
    verify_enrollment(
        &body.statement,
        &body.signature,
        EnrollmentExpectation {
            repository: &label,
            owner: &identity,
            device_id: &body.statement.device_id,
            generation: 1,
        },
    )
    .map_err(|_| enrollment_invalid())?;
    let now = now_ms();
    check_expiry(body.not_after_ms, now)?;
    repository_role(&state, &owner, &repo)
        .await
        .map_err(role_error)?;
    let authorization = DeviceAuthorization {
        version: VERSION,
        device_id: body.statement.device_id.clone(),
        repository: label,
        owner: identity,
        generation: 1,
        revoked_at_ms: None,
        allowed_check_kinds: vec![LOCAL_CI_CHECK_KIND.to_owned()],
        keys: vec![key_from(&body.statement, now, body.not_after_ms)],
    };
    let (store, repository, owner) = (state.store.clone(), repo.id, owner.id);
    let write = blocking(&state, move || {
        store.enroll_local_ci_device(
            &repository,
            &owner,
            &by,
            &authorization,
            &request,
            unix_now(),
        )
    })
    .await?;
    written(write)
}

async fn current(state: &HubState, id: &str) -> std::result::Result<LocalCiDevice, ApiError> {
    let (store, id) = (state.store.clone(), id.to_owned());
    blocking(state, move || store.local_ci_device(&id))
        .await?
        .ok_or_else(|| refusal(LocalCiRefusal::UnknownDevice))
}

fn device_revoked() -> ApiError {
    refused(
        StatusCode::CONFLICT,
        "device_revoked",
        "that device is already revoked",
    )
}

async fn change(
    state: &HubState,
    id: String,
    by: String,
    expected_generation: u64,
    next: DeviceAuthorization,
    request: LocalCiRequest,
) -> ApiResult<Value> {
    let store = state.store.clone();
    let write = blocking(state, move || {
        store.change_local_ci_device(&id, expected_generation, &by, &next, &request, unix_now())
    })
    .await?;
    written(write)
}

async fn rotate(
    State(state): State<HubState>,
    human: HumanSession,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<RotateBody>,
) -> ApiResult<Value> {
    let by = human.admin()?.user.id.clone();
    let request = request(
        &body.request_id,
        &json!({"op": "rotate", "device": id, "expected_generation": body.expected_generation,
            "not_after_ms": body.not_after_ms, "statement": body.statement,
            "signature": body.signature}),
    )?;
    if let Some(replayed) = replay(&state, &request, &id).await? {
        return Ok(replayed);
    }
    let stored = current(&state, &id).await?;
    let a = &stored.authorization;
    let produced = body
        .expected_generation
        .checked_add(1)
        .ok_or_else(ApiError::invalid)?;
    verify_enrollment(
        &body.statement,
        &body.signature,
        EnrollmentExpectation {
            repository: &a.repository,
            owner: &a.owner,
            device_id: &id,
            generation: produced,
        },
    )
    .map_err(|_| enrollment_invalid())?;
    let now = now_ms();
    // At another generation the store answers a replay or a stale generation before it
    // looks at `next`, so the current document stands in.
    let next = if a.generation == body.expected_generation {
        if a.revoked_at_ms.is_some() {
            return Err(device_revoked());
        }
        // The authorization names the label it was enrolled for. After a rename the device is
        // unusable, and a new key would be too, so rotation waits until the name matches again.
        let (store, repository) = (state.store.clone(), stored.repository.clone());
        let renamed = blocking(&state, move || store.repository_row(&repository))
            .await?
            .is_none_or(|row| local_ci_label(&row.host, &row.full_name) != a.repository);
        if renamed {
            return Err(refused(
                StatusCode::CONFLICT,
                "repository_renamed",
                "the repository was renamed since this device was enrolled; enroll a new device or rename it back",
            ));
        }
        check_expiry(body.not_after_ms, now)?;
        let key = key_from(&body.statement, now, body.not_after_ms);
        a.rotate(key, now).map_err(|_| {
            if a.keys.iter().any(|k| k.key_id == body.statement.key_id) {
                refusal(LocalCiRefusal::DuplicateKey)
            } else {
                ApiError::invalid()
            }
        })?
    } else {
        a.clone()
    };
    change(&state, id, by, body.expected_generation, next, request).await
}

async fn revoke(
    State(state): State<HubState>,
    human: HumanSession,
    Path(id): Path<String>,
    JsonBody(body): JsonBody<RevokeBody>,
) -> ApiResult<Value> {
    let by = human.admin()?.user.id.clone();
    let request = request(
        &body.request_id,
        &json!({"op": "revoke", "device": id, "expected_generation": body.expected_generation}),
    )?;
    if let Some(replayed) = replay(&state, &request, &id).await? {
        return Ok(replayed);
    }
    let stored = current(&state, &id).await?;
    let a = &stored.authorization;
    let next = if a.generation == body.expected_generation {
        if a.revoked_at_ms.is_some() {
            return Err(device_revoked());
        }
        a.revoke(now_ms()).map_err(|_| ApiError::invalid())?
    } else {
        a.clone()
    };
    change(&state, id, by, body.expected_generation, next, request).await
}

fn query(
    query: std::result::Result<Query<RepositoryQuery>, QueryRejection>,
) -> std::result::Result<String, ApiError> {
    query
        .map(|Query(query)| query.repository)
        .map_err(|_| ApiError::invalid())
}

async fn list(
    State(state): State<HubState>,
    human: HumanSession,
    repository: std::result::Result<Query<RepositoryQuery>, QueryRejection>,
) -> ApiResult<Value> {
    human.admin()?;
    let repo = repository_for(&state, &query(repository)?).await?;
    let store = state.store.clone();
    let rows = blocking(&state, move || {
        let mut rows = Vec::new();
        for device in store.local_ci_devices(&repo.id, None)? {
            let events = store.local_ci_device_events(&device.authorization.device_id)?;
            rows.push((device, events));
        }
        Ok(rows)
    })
    .await?;
    let mut devices = Vec::new();
    for (device, events) in &rows {
        let mut json = device_json(device);
        // Event data holds ids, generations and key ids only.
        json["audit"] = events
            .iter()
            .map(|event| {
                json!({"seq": event.seq, "at": event.at, "kind": event.kind,
                    "data": serde_json::from_str::<Value>(&event.data).unwrap_or(Value::Null)})
            })
            .collect();
        devices.push(json);
    }
    Ok(Json(json!({"devices": devices})))
}

/// The signed-in owner's own devices on one repository, for a device that signs with them.
/// Read only: it changes nothing. An unknown repository gives an empty list, so the route
/// doesn't reveal which repositories are connected.
async fn own(
    State(state): State<HubState>,
    owner: Owner,
    repository: std::result::Result<Query<RepositoryQuery>, QueryRejection>,
) -> ApiResult<Value> {
    let Some(repo) = find_repository(&state, &query(repository)?).await? else {
        return Ok(Json(json!({"devices": []})));
    };
    let (store, user) = (state.store.clone(), owner.user().id.clone());
    let devices = blocking(&state, move || {
        store.local_ci_devices(&repo.id, Some(&user))
    })
    .await?;
    let rows: Vec<_> = devices
        .iter()
        .map(|device| {
            json!({"device_id": device.authorization.device_id,
                "repository": device.authorization.repository,
                "generation": device.authorization.generation, "digest": device.digest,
                "revoked_at_ms": device.authorization.revoked_at_ms})
        })
        .collect();
    Ok(Json(json!({"devices": rows})))
}

/// Rechecks current authorization for an offline result: the stored device, the repository's
/// current label, the owner's standing and their role through the Hub's own connection
/// credential, never a cached answer. An unreadable or uncertain input is `Unknown`, never
/// `Current`; an unknown device id is a refusal. Issue #273 calls this; it has no route.
pub async fn authorize_offline_result(
    state: &HubState,
    claim: &DeviceClaim,
    clock: EvaluationClock,
) -> DeviceDecision {
    let unavailable = DeviceDecision::Unknown(DeviceUnknown::AuthorizationUnavailable);
    let (store, id) = (state.store.clone(), claim.device_id.clone());
    let loaded = blocking(state, move || {
        let Some(device) = store.local_ci_device(&id)? else {
            return Ok(None);
        };
        let repository = store.repository_row(&device.repository)?;
        let owner = store.user_by_id(&device.owner)?;
        Ok(Some((device, repository, owner)))
    })
    .await;
    let (device, repository, owner) = match loaded {
        Err(_) => return unavailable,
        Ok(None) => return DeviceDecision::Refused(DeviceRefusal::UnknownKey),
        Ok(Some(loaded)) => loaded,
    };
    let (Some(repository), Some(owner)) = (repository, owner) else {
        return unavailable;
    };
    // The row is stable across a rename; the authorization names the label it was enrolled for.
    if local_ci_label(&repository.host, &repository.full_name) != device.authorization.repository {
        return DeviceDecision::Refused(DeviceRefusal::WrongRepository);
    }
    let standing = if owner.disabled {
        OwnerStanding::Disabled
    } else {
        match repository_role(state, &owner, &repository).await {
            Ok(()) => OwnerStanding::Current,
            Err(Denial::Role(_)) => OwnerStanding::RoleMissing,
            Err(Denial::Kind(_)) => OwnerStanding::Unavailable,
        }
    };
    assess_device(&device.authorization, claim, standing, clock)
}
