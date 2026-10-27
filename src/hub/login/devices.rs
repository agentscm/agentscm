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
use super::{ApiResult, Human, random_hex, session_digest};
use crate::team::auth::generate_secret;

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

/// Why a mint was refused: a kind, or a role from [`super::missing_role`].
#[derive(Debug, PartialEq, Eq)]
enum Denial {
    Kind(&'static str),
    Role((&'static str, &'static str, &'static str)),
}

/// A refusal people read as is: the reason and the one fix.
fn refused(kind: &'static str) -> ApiError {
    refused_about(kind, "this repository", "the forge")
}

fn refused_about(kind: &'static str, repository: &str, forge: &str) -> ApiError {
    let (status, message) = match kind {
        "sign_in_required" => (
            StatusCode::UNAUTHORIZED,
            "This computer isn't signed in to this Hub any more; choose Sign In in the AgentSCM menu bar app or run agentscm hub login".to_owned(),
        ),
        "permission_unavailable" => (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("{forge} didn't answer the Hub's access check; nothing changed, so try again in a minute"),
        ),
        "not_connected" => (
            StatusCode::FORBIDDEN,
            format!("{repository} isn't connected to this Hub yet; ask a Hub admin to connect it"),
        ),
        "access_check_unavailable" => (
            StatusCode::FORBIDDEN,
            format!("The Hub can't check your access to {repository}; ask a Hub admin to check that repository's connection"),
        ),
        "not_allowed" => (StatusCode::FORBIDDEN, super::NOT_ALLOWED.to_owned()),
        _ => (StatusCode::FORBIDDEN, super::DISABLED.to_owned()),
    };
    ApiError::new(status, kind).with_message(message)
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
    // Only a sign-in from before computers had credentials enrolls one: a session that came
    // with a computer cannot add another after that computer was revoked.
    if human.device.is_some() {
        return Err(
            ApiError::new(StatusCode::FORBIDDEN, "device_revoked").with_message(
                "This computer was removed from your account; sign in again to add it back",
            ),
        );
    }
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
    let (store, revoked_id) = (state.store.clone(), device.clone());
    let workspace = state.workspace.clone();
    let revoked = blocking(state, move || {
        let operation = || store.revoke_device(&device, owner.as_deref(), &by, unix_now());
        match workspace {
            Some(workspace) => workspace.with_identity_change(operation),
            None => operation(),
        }
    })
    .await?;
    if revoked {
        state.login.drop_sessions_of_device(&revoked_id);
    }
    Ok(revoked)
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
    let forge = super::forge_name(&identity.provider);
    let Some(repo) = blocking(&state, move || store.repository_named(&host, &named)).await? else {
        return Err(match fork_parent(&state, &owner, &body.repository).await {
            Some(parent) => refused_about("not_connected", &body.repository, forge)
                .with_message(format!(
                    "{} isn't connected to this Hub, but it is a fork of {parent}, which is; make {parent} this checkout's origin remote to coordinate",
                    body.repository
                ))
                .with_field("fork_of", json!(parent)),
            None => refused_about("not_connected", &body.repository, forge),
        });
    };
    match repository_role(&state, &owner, &repo).await {
        Ok(()) => {}
        Err(Denial::Role(role)) => {
            record_refusal(&state, &owner, "insufficient_role", "device_mint").await;
            return Err(super::insufficient_role(
                &owner.login,
                &repo.full_name,
                role,
            ));
        }
        Err(Denial::Kind(kind)) => {
            if kind != "permission_unavailable" && kind != "not_connected" {
                record_refusal(&state, &owner, kind, "device_mint").await;
            }
            return Err(refused_about(kind, &repo.full_name, forge));
        }
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

/// The connection credential for reads on `host` by `provider`: the first enabled connection
/// there, with one of its repositories for a GitHub App's installation.
async fn host_credential(
    state: &HubState,
    provider: &str,
    host: &str,
) -> Option<(super::super::connection::Connection, String)> {
    let (store, provider, host) = (state.store.clone(), provider.to_owned(), host.to_owned());
    let (conn, installation) = blocking(state, move || {
        for conn in store.connections()? {
            if conn.provider == provider && conn.host == host && conn.status == "enabled" {
                let installation = store
                    .repositories(&conn.id)?
                    .into_iter()
                    .find_map(|repo| repo.installation_id);
                return Ok(Some((conn, installation)));
            }
        }
        Ok(None)
    })
    .await
    .ok()??;
    let token = state.sync.token(&conn, installation).await.ok()?;
    Some((conn, token))
}

/// The connected repository `repository` was forked from, when the forge says so. Asked only
/// for a repository this Hub does not have, so a contributor working in a fork is told which
/// remote to use instead of only "not connected". Never guesses: no answer is no hint.
async fn fork_parent(state: &HubState, owner: &User, repository: &str) -> Option<String> {
    let identity = &owner.identity;
    let (conn, token) = host_credential(state, &identity.provider, &identity.host).await?;
    let reply = match state.sync.forge(&conn).ok()? {
        ForgeApi::Github(api) => api.repo_json(&token, repository).await,
        ForgeApi::Gitlab(api) => api.project_json(&token, repository).await,
    }
    .ok()??;
    let parents = [
        &reply["parent"]["full_name"],
        &reply["source"]["full_name"],
        &reply["forked_from_project"]["path_with_namespace"],
    ];
    for parent in parents.into_iter().filter_map(Value::as_str) {
        let (store, host, named) = (
            state.store.clone(),
            identity.host.clone(),
            parent.to_owned(),
        );
        if let Ok(Some(connected)) =
            blocking(state, move || store.repository_named(&host, &named)).await
        {
            return Some(connected.full_name);
        }
    }
    None
}

/// The person's current role on a connected repository, asked with the connection's own
/// credential and judged by [`super::missing_role`], the rule sign-in and session minting use.
///
/// The only source today is the forge; an administrator-issued grant for other Git hosts would
/// be a second source here.
async fn repository_role(
    state: &HubState,
    owner: &User,
    repo: &ConnectedRepository,
) -> std::result::Result<(), Denial> {
    let kind = Denial::Kind;
    let store = state.store.clone();
    let id = repo.connection.clone();
    let conn = match blocking(state, move || store.connection(&id)).await {
        Ok(Some(conn)) => conn,
        Ok(None) => return Err(kind("not_connected")),
        Err(_) => return Err(kind("permission_unavailable")),
    };
    if conn.status == "auth_failed" || state.sync.config().is_err() {
        return Err(kind("access_check_unavailable"));
    }
    let forge_error = |error: GhError| match error {
        GhError::Auth(_) => kind("access_check_unavailable"),
        GhError::Limited(_) | GhError::Failed(_) => kind("permission_unavailable"),
    };
    let token = state
        .sync
        .token(&conn, repo.installation_id)
        .await
        .map_err(forge_error)?;
    let api = state
        .sync
        .forge(&conn)
        .map_err(|_| kind("access_check_unavailable"))?;
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
    // The connection's own credential can no longer see the repository.
    let Some((project, role)) = reply else {
        return Err(kind("access_check_unavailable"));
    };
    if project["id"].as_i64() != Some(repo.repository_id) {
        return Err(kind("not_connected"));
    }
    let gitlab = matches!(api, ForgeApi::Gitlab(_));
    // The GitHub login is cached from sign-in: an unknown one was renamed, and one with another
    // id now belongs to someone else. Signing in again refreshes it.
    let user_id = match (&role, gitlab) {
        (Some(role), true) => role["id"].as_i64(),
        (Some(role), false) => role["user"]["id"].as_i64(),
        (None, true) => Some(owner.identity.user_id),
        (None, false) => None,
    };
    if user_id != Some(owner.identity.user_id) {
        return Err(kind("sign_in_required"));
    }
    match role_refusal(gitlab, &project, role.as_ref()) {
        None => Ok(()),
        Some(role) => Err(Denial::Role(role)),
    }
}

/// The forge's role reply in the shape of a person's own repository read, judged by
/// [`super::missing_role_on`]. No role at all is refused on any repository.
fn role_refusal(
    gitlab: bool,
    project: &Value,
    role: Option<&Value>,
) -> Option<(&'static str, &'static str, &'static str)> {
    if gitlab {
        let visibility = match project["visibility"].as_str() {
            Some("private") => "private",
            Some("internal") => "internal",
            _ => "public",
        };
        let level = role
            .filter(|member| {
                member["state"]
                    .as_str()
                    .is_none_or(|state| state == "active")
            })
            .and_then(|member| member["access_level"].as_i64());
        let shaped = json!({"visibility": visibility,
            "permissions": {"project_access": level.map(|l| json!({"access_level": l}))}});
        return super::missing_role_on(true, &shaped);
    }
    let permission = role
        .and_then(|role| role["permission"].as_str())
        .unwrap_or("none");
    let name = role
        .and_then(|role| role["role_name"].as_str())
        .unwrap_or_default();
    let public = project["private"] == json!(false);
    if permission == "none" {
        return Some(if public {
            ("public", "none", "triage")
        } else {
            ("private", "none", "read")
        });
    }
    // A custom role is judged by its base permission, which fails closed.
    let shaped = json!({"private": project["private"], "permissions": {
        "push": matches!(permission, "write" | "admin") || matches!(name, "write" | "maintain" | "admin"),
        "triage": name == "triage", "maintain": name == "maintain", "admin": permission == "admin"}});
    super::missing_role_on(false, &shaped)
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
    fn github_roles_follow_missing_role() {
        let private = json!({"private": true});
        let public = json!({"private": false});
        let role =
            |permission: &str, name: &str| json!({"permission": permission, "role_name": name});
        let refused =
            |project: &Value, role: Option<Value>| role_refusal(false, project, role.as_ref());
        assert_eq!(refused(&private, Some(role("read", "read"))), None);
        assert_eq!(
            refused(&private, Some(role("none", ""))),
            Some(("private", "none", "read"))
        );
        assert_eq!(refused(&private, None), Some(("private", "none", "read")));
        assert_eq!(
            refused(&public, Some(role("read", "read"))),
            Some(("public", "read", "triage"))
        );
        assert_eq!(refused(&public, Some(role("read", "triage"))), None);
        assert_eq!(refused(&public, Some(role("write", "custom-writer"))), None);
        // A custom role built on read fails closed on a public repository.
        assert!(refused(&public, Some(role("read", "custom-reader"))).is_some());
        // A reply that does not say public counts as private, as sign-in does.
        assert_eq!(refused(&json!({}), Some(role("read", "read"))), None);
    }

    #[test]
    fn gitlab_levels_follow_missing_role() {
        let project = |visibility: &str| json!({"visibility": visibility});
        let member = |level: i64| json!({"access_level": level, "state": "active"});
        let refused =
            |project: &Value, member: Option<Value>| role_refusal(true, project, member.as_ref());
        assert_eq!(refused(&project("private"), Some(member(20))), None);
        assert_eq!(
            refused(&project("private"), Some(member(10))),
            Some(("private", "guest", "reporter"))
        );
        assert_eq!(
            refused(&project("internal"), Some(member(20))),
            Some(("internal", "reporter", "developer"))
        );
        assert_eq!(refused(&project("internal"), Some(member(30))), None);
        assert!(
            refused(
                &project("private"),
                Some(json!({"access_level": 50, "state": "blocked"}))
            )
            .is_some()
        );
        assert_eq!(
            refused(&project("private"), None),
            Some(("private", "none", "reporter"))
        );
    }

    #[test]
    fn every_refusal_has_a_reason_and_a_fix() {
        for kind in [
            "sign_in_required",
            "permission_unavailable",
            "not_connected",
            "access_check_unavailable",
            "not_allowed",
            "user_disabled",
        ] {
            let error = refused_about(kind, "acme/widgets", "GitHub");
            let message = error.message().unwrap();
            assert!(message.contains("; "), "{kind}: {message}");
            assert_eq!(error.code(), kind);
        }
    }
}
