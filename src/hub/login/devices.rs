//! Computer credentials: issued at sign-in, they mint and renew their owner's agent tokens with
//! the repository role re-checked through the Hub's own connection credential, never a stored
//! user forge token. They are refused on every human-session and agent route.
use axum::Router;
use axum::extract::{FromRequestParts, Path, State};
use axum::http::StatusCode;
use axum::http::request::Parts;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use serde::Deserialize;
use serde_json::{Value, json};

use super::super::connection::ConnectedRepository;
use super::super::device::{
    DEVICE_AGENT_TOKEN_TTL_SECONDS, DEVICE_READ_TOKEN_TTL_SECONDS, Device, DevicePrincipal,
    Refusal, validate_device_name,
};
use super::super::forge_api::ForgeApi;
use super::super::github::GhError;
use super::super::http::{ApiError, JsonBody, bearer, blocking};
use super::super::store::{
    NewAgentToken, User, agent_token_digest, device_digest, unix_now, validate_agent_name,
    validate_repository,
};
use super::super::{HubError, HubState};
use super::{ApiResult, GITLAB_DEVELOPER, GITLAB_REPORTER, Human, random_hex, session_digest};
use crate::team::auth::generate_secret;

/// GitHub roles that may use a public repository (`may_use`: push, triage, maintain, admin).
const PUBLIC_ROLES: [&str; 4] = ["triage", "write", "maintain", "admin"];

pub(super) fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/device", get(status).delete(sign_out))
        .route("/v1/hub/device/name", post(rename))
        .route("/v1/hub/device/tokens", post(mint))
        .route("/v1/hub/device/tokens/{id}", delete(revoke_token))
        .route("/v1/hub/devices", get(list).post(enroll))
        .route("/v1/hub/devices/{id}", delete(revoke))
        .route("/v1/hub/admin/devices", get(admin_devices))
        .route("/v1/hub/admin/repositories", get(admin_repositories))
        .route("/v1/hub/admin/sign-in-refusals", get(admin_refusals))
}

fn refused(kind: &'static str) -> ApiError {
    let status = match kind {
        "sign_in_required" => StatusCode::UNAUTHORIZED,
        "permission_unavailable" => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::FORBIDDEN,
    };
    ApiError::new(status, kind)
}

/// A live computer credential, from `Authorization: Bearer <secret>`.
pub(super) struct DeviceAuth(DevicePrincipal);

impl FromRequestParts<HubState> for DeviceAuth {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &HubState,
    ) -> std::result::Result<Self, ApiError> {
        let digest = device_digest(bearer(parts)?).map_err(|_| refused("sign_in_required"))?;
        let store = state.store.clone();
        let found = blocking(state, move || {
            store.authenticate_device(&digest, unix_now())
        })
        .await?;
        match found {
            None => Err(refused("sign_in_required")),
            Some(device) if device.owner.disabled => Err(refused("user_disabled")),
            Some(device) => Ok(Self(device)),
        }
    }
}

/// A human session or a computer credential: the routes a person uses about their computers.
enum Owner {
    Human(Human),
    Device(DevicePrincipal),
}

impl Owner {
    fn user(&self) -> &User {
        match self {
            Self::Human(human) => &human.user,
            Self::Device(device) => &device.owner,
        }
    }
}

impl FromRequestParts<HubState> for Owner {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &HubState,
    ) -> std::result::Result<Self, ApiError> {
        let raw = bearer(parts)?;
        if let Some(human) = session_digest(raw)
            .ok()
            .and_then(|digest| state.login.session(&digest))
        {
            return Ok(Self::Human(human));
        }
        DeviceAuth::from_request_parts(parts, state)
            .await
            .map(|DeviceAuth(device)| Self::Device(device))
    }
}

/// What a client asks for with a sign-in: a new computer, optionally replacing the one whose
/// credential it still holds.
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeviceRequest {
    /// Default: "<login>'s computer".
    #[serde(default)]
    pub(super) name: Option<String>,
    /// The secret of this computer's previous credential; only that computer is replaced.
    #[serde(default)]
    pub(super) replaces: Option<String>,
}

/// Creates a computer for `user` and returns `{id, secret, name}`; the secret is shown once.
pub(super) async fn issue(
    state: &HubState,
    user: &User,
    request: DeviceRequest,
) -> ApiResult<Value> {
    let name = match request.name {
        Some(name) => name,
        None => format!("{}'s computer", user.login)
            .chars()
            .take(64)
            .collect(),
    };
    validate_device_name(&name).map_err(|_| ApiError::invalid())?;
    let secret = random_hex()?;
    let digest = device_digest(&secret)?;
    let (store, owner, stored) = (state.store.clone(), user.id.clone(), name.clone());
    let replaces = request.replaces;
    let id = blocking(state, move || {
        let now = unix_now();
        // A credential that is not this person's live computer replaces nothing.
        let replaces = match replaces.as_deref().map(device_digest) {
            Some(Ok(old)) => store
                .authenticate_device(&old, now)?
                .filter(|device| device.owner.id == owner)
                .map(|device| device.device_id),
            _ => None,
        };
        store.insert_device(&owner, &stored, &digest, replaces.as_deref(), now)
    })
    .await?;
    Ok(json!({"id": id, "secret": secret, "name": name}))
}

/// Records a refusal for administrators. Best effort: a refusal is never turned into success.
pub(super) async fn record_refusal(state: &HubState, user: &User, reason: &str, via: &'static str) {
    let refusal = Refusal {
        provider: user.identity.provider.clone(),
        host: user.identity.host.clone(),
        forge_user_id: user.identity.user_id,
        login: user.login.clone(),
        reason: reason.to_owned(),
        via: via.to_owned(),
        at: unix_now(),
        count: 1,
    };
    let store = state.store.clone();
    if blocking(state, move || store.record_refusal(&refusal))
        .await
        .is_err()
    {
        eprintln!("hub: a sign-in refusal was not recorded");
    }
}

fn device_json(device: &Device, current: Option<&str>) -> Value {
    json!({"id": device.id, "name": device.name, "created_at": device.created_at,
        "last_used_at": device.last_used_at, "revoked_at": device.revoked_at,
        "current": current == Some(device.id.as_str()), "repositories": device.repositories})
}

async fn status(DeviceAuth(device): DeviceAuth) -> Json<Value> {
    Json(json!({
        "device": {"id": device.device_id, "name": device.name,
            "created_at": device.created_at, "last_used_at": device.last_used_at},
        "user": super::user_json(&device.owner),
    }))
}

async fn sign_out(
    State(state): State<HubState>,
    DeviceAuth(device): DeviceAuth,
) -> ApiResult<StatusCode> {
    revoke_device(
        &state,
        device.device_id.clone(),
        Some(device.owner.id),
        device.device_id,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameBody {
    name: String,
}

async fn rename(
    State(state): State<HubState>,
    DeviceAuth(device): DeviceAuth,
    JsonBody(body): JsonBody<RenameBody>,
) -> ApiResult<StatusCode> {
    validate_device_name(&body.name).map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    blocking(&state, move || {
        store.rename_device(&device.device_id, &device.owner.id, &body.name)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A person with a live session enrolls this computer without signing in again.
async fn enroll(
    State(state): State<HubState>,
    super::HumanSession(human): super::HumanSession,
    JsonBody(body): JsonBody<DeviceRequest>,
) -> ApiResult<Response> {
    let device = issue(&state, &human.user, body).await?;
    Ok((StatusCode::CREATED, Json(json!({"device": device}))).into_response())
}

async fn list(State(state): State<HubState>, owner: Owner) -> ApiResult<Json<Value>> {
    let current = match &owner {
        Owner::Device(device) => Some(device.device_id.clone()),
        Owner::Human(_) => None,
    };
    let (store, id) = (state.store.clone(), owner.user().id.clone());
    let devices = blocking(&state, move || store.devices(Some(&id), unix_now())).await?;
    let devices: Vec<_> = devices
        .iter()
        .map(|device| device_json(device, current.as_deref()))
        .collect();
    Ok(Json(json!({"devices": devices})))
}

async fn revoke_device(
    state: &HubState,
    device: String,
    owner: Option<String>,
    by: String,
) -> ApiResult<bool> {
    let store = state.store.clone();
    let workspace = state.workspace.clone();
    Ok(blocking(state, move || {
        let operation = || store.revoke_device(&device, owner.as_deref(), &by, unix_now());
        match workspace {
            Some(workspace) => workspace.with_identity_change(operation),
            None => operation(),
        }
    })
    .await?)
}

/// The owner (or an administrator's session) revokes a computer and every token it minted.
async fn revoke(
    State(state): State<HubState>,
    owner: Owner,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let (scope, by) = match &owner {
        Owner::Device(device) if device.device_id == id => return Err(ApiError::invalid()),
        Owner::Device(device) => (Some(device.owner.id.clone()), device.device_id.clone()),
        Owner::Human(human) if human.admin => (None, human.user.id.clone()),
        Owner::Human(human) => (Some(human.user.id.clone()), human.user.id.clone()),
    };
    if revoke_device(&state, id, scope, by).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MintBody {
    name: String,
    repository: String,
    #[serde(default)]
    read_only: bool,
    /// The owner's token for the same repository that this one renews.
    #[serde(default)]
    replaces: Option<String>,
    /// Asked by auto-connect for a checkout that had no connection.
    #[serde(default)]
    auto: bool,
}

/// Mint or renew an agent token. Every call re-checks the allow list and the person's current
/// role on the repository; a refusal mints nothing and leaves the current token alone.
async fn mint(
    State(state): State<HubState>,
    DeviceAuth(device): DeviceAuth,
    JsonBody(body): JsonBody<MintBody>,
) -> ApiResult<Response> {
    validate_agent_name(&body.name)?;
    validate_repository(&body.repository)?;
    if body
        .replaces
        .as_deref()
        .is_some_and(|id| !id.starts_with("tok_") || id.len() > 64)
    {
        return Err(ApiError::invalid());
    }
    let owner = device.owner.clone();
    let identity = &owner.identity;
    let login = &state.login;
    if login.has_allow_list()
        && !login.is_admin(&identity.provider, identity.user_id, &owner.login)
        && !login.is_allowed(&identity.provider, identity.user_id, &owner.login)
    {
        record_refusal(&state, &owner, "not_allowed", "device_mint").await;
        return Err(refused("not_allowed"));
    }
    let (store, host, named) = (
        state.store.clone(),
        identity.host.clone(),
        body.repository.clone(),
    );
    let Some(repo) = blocking(&state, move || store.repository_named(&host, &named)).await? else {
        return Err(refused("not_connected"));
    };
    if let Err(kind) = repository_role(&state, &owner, &repo).await {
        if kind != "permission_unavailable" && kind != "not_connected" {
            record_refusal(&state, &owner, kind, "device_mint").await;
        }
        return Err(refused(kind));
    }
    let secret = generate_secret().map_err(|_| HubError::Unavailable("random source".into()))?;
    let ttl = if body.read_only {
        DEVICE_READ_TOKEN_TTL_SECONDS
    } else {
        DEVICE_AGENT_TOKEN_TTL_SECONDS
    };
    let token = NewAgentToken {
        owner: owner.id.clone(),
        name: body.name.clone(),
        repository: repo.full_name.clone(),
        digest: agent_token_digest(secret.expose())?,
        expires_at: unix_now() + ttl,
        repository_id: Some(repo.repository_id),
        read_only: body.read_only,
    };
    let (store, device_id, expires_at) = (
        state.store.clone(),
        device.device_id.clone(),
        token.expires_at,
    );
    let replaces = body.replaces.clone();
    let id = blocking(&state, move || {
        store.insert_device_token(&token, &device_id, replaces.as_deref(), body.auto)
    })
    .await
    .map_err(|error| match error {
        HubError::Unauthorized => refused("sign_in_required"),
        error => error.into(),
    })?;
    let reply = json!({"id": id, "token": secret.expose(), "name": body.name,
        "repository": repo.full_name, "repository_id": repo.repository_id,
        "expires_at": expires_at, "read_only": body.read_only, "device_id": device.device_id});
    Ok((StatusCode::CREATED, Json(reply)).into_response())
}

async fn revoke_token(
    State(state): State<HubState>,
    DeviceAuth(device): DeviceAuth,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let store = state.store.clone();
    let workspace = state.workspace.clone();
    let revoked = blocking(&state, move || {
        let operation = || store.revoke_owned_agent_token(&device.owner.id, &id);
        match workspace {
            Some(workspace) => workspace.with_identity_change(operation),
            None => operation(),
        }
    })
    .await?;
    if revoked {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::not_found())
    }
}

/// The person's current role on a connected repository, asked with the connection's own
/// credential and judged by the `may_use` rule. Err is the refusal kind.
///
/// The only source today is the forge; an administrator-issued grant for other Git hosts would
/// be a second source here.
async fn repository_role(
    state: &HubState,
    owner: &User,
    repo: &ConnectedRepository,
) -> std::result::Result<(), &'static str> {
    let store = state.store.clone();
    let id = repo.connection.clone();
    let conn = match blocking(state, move || store.connection(&id)).await {
        Ok(Some(conn)) => conn,
        Ok(None) => return Err("not_connected"),
        Err(_) => return Err("permission_unavailable"),
    };
    if conn.status == "auth_failed" || state.sync.config().is_err() {
        return Err("access_check_unavailable");
    }
    let forge_error = |error: GhError| match error {
        GhError::Auth(_) => "access_check_unavailable",
        GhError::Limited(_) | GhError::Failed(_) => "permission_unavailable",
    };
    let token = state
        .sync
        .token(&conn, repo.installation_id)
        .await
        .map_err(forge_error)?;
    let api = state
        .sync
        .forge(&conn)
        .map_err(|_| "access_check_unavailable")?;
    let reply = match api {
        ForgeApi::Github(api) => {
            api.collaborator_permission(&token, &repo.full_name, &owner.login)
                .await
        }
        ForgeApi::Gitlab(api) => {
            api.member_access(&token, repo.repository_id, owner.identity.user_id)
                .await
        }
    }
    .map_err(forge_error)?;
    let Some((project, role)) = reply else {
        return Err("no_repository_role");
    };
    if project["id"].as_i64() != Some(repo.repository_id) {
        return Err("not_connected");
    }
    let permitted = match api {
        ForgeApi::Github(_) => {
            // The login is cached from sign-in; a renamed or re-registered one is someone else.
            if role["user"]["id"].as_i64() != Some(owner.identity.user_id) {
                return Err("sign_in_required");
            }
            github_permits(&project, &role)
        }
        ForgeApi::Gitlab(_) => {
            if role["id"].as_i64() != Some(owner.identity.user_id) {
                return Err("no_repository_role");
            }
            gitlab_permits(&project, &role)
        }
    };
    if permitted {
        Ok(())
    } else {
        Err("no_repository_role")
    }
}

/// `may_use` for the collaborator API: any role on a private repository, triage or more on a
/// public one. A custom role is judged by its base permission, which fails closed.
fn github_permits(repo: &Value, role: &Value) -> bool {
    let permission = role["permission"].as_str().unwrap_or("none");
    if repo["private"] != json!(false) {
        return matches!(permission, "read" | "write" | "admin");
    }
    role["role_name"]
        .as_str()
        .is_some_and(|name| PUBLIC_ROLES.contains(&name))
        || matches!(permission, "write" | "admin")
}

/// `may_use` for GitLab members: Reporter on a private project, Developer otherwise. A blocked
/// or pending membership does not count.
fn gitlab_permits(project: &Value, member: &Value) -> bool {
    if member["state"]
        .as_str()
        .is_some_and(|state| state != "active")
    {
        return false;
    }
    let needed = if project["visibility"] == json!("private") {
        GITLAB_REPORTER
    } else {
        GITLAB_DEVELOPER
    };
    member["access_level"]
        .as_i64()
        .is_some_and(|level| level >= needed)
}

async fn admin_devices(
    State(state): State<HubState>,
    human: super::HumanSession,
) -> ApiResult<Json<Value>> {
    human.admin()?;
    let store = state.store.clone();
    let devices = blocking(&state, move || store.devices(None, unix_now())).await?;
    let devices: Vec<_> = devices
        .iter()
        .map(|device| {
            let mut json = device_json(device, None);
            json["owner"] = json!({"id": device.owner.id, "login": device.owner.login,
                "provider": device.owner.identity.provider});
            json.as_object_mut().map(|object| object.remove("current"));
            json
        })
        .collect();
    Ok(Json(json!({"devices": devices})))
}

async fn admin_repositories(
    State(state): State<HubState>,
    human: super::HumanSession,
) -> ApiResult<Json<Value>> {
    human.admin()?;
    let store = state.store.clone();
    let rows = blocking(&state, move || {
        let providers: std::collections::HashMap<String, String> = store
            .connections()?
            .into_iter()
            .map(|conn| (conn.id, conn.provider))
            .collect();
        Ok((store.repository_usage(unix_now())?, providers))
    })
    .await?;
    let (usage, providers) = rows;
    let repositories: Vec<_> = usage
        .iter()
        .map(|repo| {
            json!({"full_name": repo.full_name, "host": repo.host,
                "provider": providers.get(&repo.connection), "connection": repo.connection,
                "devices": repo.devices, "agent_tokens": repo.agent_tokens})
        })
        .collect();
    Ok(Json(json!({"repositories": repositories})))
}

async fn admin_refusals(
    State(state): State<HubState>,
    human: super::HumanSession,
) -> ApiResult<Json<Value>> {
    human.admin()?;
    let store = state.store.clone();
    let rows = blocking(&state, move || store.refusals(unix_now())).await?;
    let refusals: Vec<_> = rows
        .iter()
        .map(|r| {
            json!({"at": r.at, "count": r.count, "provider": r.provider, "host": r.host,
                "login": r.login, "forge_user_id": r.forge_user_id, "reason": r.reason,
                "via": r.via})
        })
        .collect();
    Ok(Json(json!({"refusals": refusals})))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_roles_follow_may_use() {
        let private = json!({"private": true});
        let public = json!({"private": false});
        let role =
            |permission: &str, name: &str| json!({"permission": permission, "role_name": name});
        assert!(github_permits(&private, &role("read", "read")));
        assert!(!github_permits(&private, &role("none", "")));
        assert!(!github_permits(&public, &role("read", "read")));
        assert!(github_permits(&public, &role("read", "triage")));
        assert!(github_permits(&public, &role("write", "custom-writer")));
        // A custom role built on read fails closed on a public repository.
        assert!(!github_permits(&public, &role("read", "custom-reader")));
        // A reply that does not say public counts as private, as `may_use` does.
        assert!(github_permits(&json!({}), &role("read", "read")));
    }

    #[test]
    fn gitlab_levels_follow_may_use() {
        let private = json!({"visibility": "private"});
        let internal = json!({"visibility": "internal"});
        let member = |level: i64| json!({"access_level": level, "state": "active"});
        assert!(gitlab_permits(&private, &member(20)));
        assert!(!gitlab_permits(&private, &member(10)));
        assert!(!gitlab_permits(&internal, &member(20)));
        assert!(gitlab_permits(&internal, &member(30)));
        assert!(!gitlab_permits(
            &private,
            &json!({"access_level": 50, "state": "blocked"})
        ));
        assert!(!gitlab_permits(&private, &json!({})));
    }
}
