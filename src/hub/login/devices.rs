//! Computer credentials: issued at sign-in, they mint and renew their owner's agent tokens with
//! the repository role re-checked through the Hub's own connection credential, never a stored
//! user forge token. They are refused on every human-session and agent route.
use std::time::Duration;

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
use super::super::repository_summary::{Facts, summarize};
use super::super::store::{
    NewAgentToken, Principal, User, agent_token_digest, device_digest, mint_web_read_token,
    unix_now, validate_agent_name, validate_repository,
};
use super::super::work::{ClaimStatus, WorkOverviewQuery, WorkState};
use super::super::{HubError, HubState};
use super::{ApiResult, Human, Login, random_hex, session_digest};
use crate::team::auth::generate_secret;

pub(super) fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/device", get(status).delete(sign_out))
        .route("/v1/hub/device/name", post(rename))
        .route("/v1/hub/device/tokens", post(mint))
        .route("/v1/hub/device/tokens/{id}", delete(revoke_token))
        .route("/v1/hub/session/read-token", post(web_read_token))
        .route("/v1/hub/session/repositories", get(session_repositories))
        .route("/v1/hub/device/repositories", get(device_repositories))
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
        // The web page's cookie sign-in, with its CSRF check.
        if !parts
            .headers
            .contains_key(axum::http::header::AUTHORIZATION)
        {
            return super::HumanSession::from_request_parts(parts, state)
                .await
                .map(|super::HumanSession(human)| Self::Human(human));
        }
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
    let (replaces, workspace) = (request.replaces, state.workspace.clone());
    let (id, replaced) = blocking(state, move || {
        let now = unix_now();
        // A credential that is not this person's live computer replaces nothing.
        let replaced = match replaces.as_deref().map(device_digest) {
            Some(Ok(old)) => store
                .authenticate_device(&old, now)?
                .filter(|device| device.owner.id == owner)
                .map(|device| device.device_id),
            _ => None,
        };
        // Replacing revokes agent tokens, so it waits for native actions as revocation does.
        let operation = || store.insert_device(&owner, &stored, &digest, replaced.as_deref(), now);
        let id = match (&workspace, &replaced) {
            (Some(workspace), Some(_)) => workspace.with_identity_change(operation),
            _ => operation(),
        }?;
        Ok((id, replaced))
    })
    .await?;
    if let Some(replaced) = replaced {
        state.login.drop_sessions_of_device(&replaced);
    }
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
    // A web page never holds a computer credential: it would outlive the page's sign-in.
    if human.browser.is_some() {
        return Err(super::forbidden()
            .with_message("A computer signs in with `agentscm hub login`, not from a web page"));
    }
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
    // The session now came with this computer: it cannot enroll another, and revoking this one
    // ends it.
    if let Some((_, session)) = state.login.pending().sessions.0.get_mut(&human.key) {
        session.device = device["id"].as_str().map(str::to_owned);
    }
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
    let owner = device.owner.clone();
    let repo = mint_check(&state, &owner, &body.repository, "device_mint").await?;
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
    let id = blocking(&state, move || {
        store.insert_device_token(&token, &device_id, body.auto)
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

/// The connected repository `repository` when `owner` may have a token for it now: the allow
/// list and their current role, checked for every mint. A refusal is recorded under `via`.
async fn mint_check(
    state: &HubState,
    owner: &User,
    repository: &str,
    via: &'static str,
) -> ApiResult<ConnectedRepository> {
    allow_listed(state, owner, via).await?;
    let identity = &owner.identity;
    let (store, host, named) = (
        state.store.clone(),
        identity.host.clone(),
        repository.to_owned(),
    );
    let forge = super::forge_name(&identity.provider);
    let Some(repo) = blocking(state, move || store.repository_named(&host, &named)).await? else {
        return Err(match fork_parent(state, owner, repository).await {
            Some(parent) => refused_about("not_connected", repository, forge)
                .with_message(format!(
                    "{repository} isn't connected to this Hub, but it is a fork of {parent}, which is; make {parent} this checkout's origin remote to coordinate"
                ))
                .with_field("fork_of", json!(parent)),
            None => refused_about("not_connected", repository, forge),
        });
    };
    match repository_role(state, owner, &repo).await {
        Ok(()) => {}
        Err(Denial::Role(role)) => {
            record_refusal(state, owner, "insufficient_role", via).await;
            return Err(super::insufficient_role(
                &owner.login,
                &repo.full_name,
                role,
            ));
        }
        Err(Denial::Kind(kind)) => {
            if kind != "permission_unavailable" && kind != "not_connected" {
                record_refusal(state, owner, kind, via).await;
            }
            return Err(refused_about(kind, &repo.full_name, forge));
        }
    }
    Ok(repo)
}

/// Refuses someone the allow list leaves out, recording it under `via`.
async fn allow_listed(state: &HubState, owner: &User, via: &'static str) -> ApiResult<()> {
    let (identity, login) = (&owner.identity, &state.login);
    if login.has_allow_list()
        && !login.is_admin(&identity.provider, identity.user_id, &owner.login)
        && !login.is_allowed(&identity.provider, identity.user_id, &owner.login)
    {
        record_refusal(state, owner, "not_allowed", via).await;
        return Err(refused("not_allowed"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadTokenBody {
    repository: String,
}

/// A 30-minute read-only token for the Hub's own web pages, so a web sign-in reads the
/// dashboard without a pasted token. Only the cookie sign-in (with its CSRF token) asks, under
/// the same checks as a computer's mint.
async fn web_read_token(
    State(state): State<HubState>,
    super::HumanSession(human): super::HumanSession,
    JsonBody(body): JsonBody<ReadTokenBody>,
) -> ApiResult<Response> {
    if human.browser.is_none() {
        return Err(super::forbidden().with_message(
            "Only the Hub's web pages ask for this; agents get tokens through this computer's sign-in",
        ));
    }
    validate_repository(&body.repository)?;
    let repo = mint_check(&state, &human.user, &body.repository, "web_read_token").await?;
    let store = state.store.clone();
    let owner = human.user.id.clone();
    let (secret, expires_at) = blocking(&state, move || {
        let (_, secret, expires_at) = mint_web_read_token(
            &*store,
            &owner,
            &repo.full_name,
            repo.repository_id,
            DEVICE_READ_TOKEN_TTL_SECONDS,
        )?;
        Ok((secret.expose().to_owned(), expires_at))
    })
    .await?;
    let reply = json!({"token": secret, "expires_at": expires_at});
    Ok((StatusCode::CREATED, Json(reply)).into_response())
}

/// At most this many role checks of one repository list are with the forge at once.
const LIST_CONCURRENCY: usize = 8;
/// A repository list stops waiting for the forge after this long; the rest count as unchecked.
const LIST_DEADLINE: Duration = Duration::from_secs(10);
/// Live intents and sessions read per repository for its counts.
const LIST_LIVE_LIMIT: usize = 500;

/// The connected repositories on the person's forge host that they can read now, with a few
/// counts each, for the web pages' All repositories list. Each passes the role check a mint makes,
/// with no token stored. A refused one is left out without a refusal row; one the forge could not
/// answer for is only counted, never named.
async fn session_repositories(
    State(state): State<HubState>,
    super::HumanSession(human): super::HumanSession,
) -> ApiResult<Json<Value>> {
    if human.browser.is_none() {
        return Err(super::forbidden().with_message(
            "Only the Hub's web pages ask for this; agents get tokens through this computer's sign-in",
        ));
    }
    repositories_for_owner(state, human.user, false).await
}

/// A signed-in computer can inspect the same repositories for its tray work view. The server
/// checks each repository's current forge role before returning any names or work details.
async fn device_repositories(
    State(state): State<HubState>,
    DeviceAuth(device): DeviceAuth,
) -> ApiResult<Json<Value>> {
    repositories_for_owner(state, device.owner, true).await
}

async fn repositories_for_owner(
    state: HubState,
    owner: User,
    include_work: bool,
) -> ApiResult<Json<Value>> {
    allow_listed(
        &state,
        &owner,
        if include_work {
            "device_repositories"
        } else {
            "session_repositories"
        },
    )
    .await?;
    let (store, host) = (state.store.clone(), owner.identity.host.clone());
    let mut candidates = blocking(&state, move || {
        store.repositories_on(&host, super::MAX_SIGN_IN_PROBES + 1)
    })
    .await?;
    let complete = candidates.len() <= super::MAX_SIGN_IN_PROBES;
    candidates.truncate(super::MAX_SIGN_IN_PROBES);
    let (readable, unchecked) = readable(&state, &owner, candidates, !include_work).await;
    let store = state.store.clone();
    let mut rows = blocking(&state, move || {
        let now = unix_now();
        let revision_key = if include_work {
            const KEY: &str = "device_attention_revision_key_v1";
            let key = match store.get_setting(KEY)? {
                Some(key) => key,
                None => {
                    let candidate = generate_secret()
                        .map_err(|_| HubError::Unavailable("attention marker key".into()))?;
                    store.get_or_create_setting(KEY, candidate.expose(), "system")?
                }
            };
            (key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(key)
        } else {
            None
        };
        let mut rows = Vec::with_capacity(readable.len());
        for repo in readable {
            let overview = store.repository_work_overview(
                &owner,
                &repo.full_name,
                repo.repository_id,
                &WorkOverviewQuery::default(),
                now,
            )?;
            // `live_sessions` and `repository_events` read only the repository and forge of a
            // principal: this one names the repository the role check just allowed, with no token.
            let scope = Principal {
                token_id: String::new(),
                name: String::new(),
                repository: repo.full_name.clone(),
                repository_id: Some(repo.repository_id),
                expires_at: now,
                read_only: true,
                owner: owner.clone(),
            };
            let intents = store.live_intents_scoped(&scope, now, LIST_LIVE_LIMIT + 1)?;
            let sessions = store.live_sessions(&scope, now, LIST_LIVE_LIMIT + 1)?;
            let last_event_at = store
                .repository_events(&scope, None, 1)?
                .first()
                .map(|row| row.event.at);
            let facts = Facts {
                live_complete: intents.len() <= LIST_LIVE_LIMIT
                    && sessions.len() <= LIST_LIVE_LIMIT,
                overview,
                intents,
                sessions,
                last_event_at,
            };
            const PANEL_WORK_LIMIT: usize = 20;
            const PANEL_LIVE_LIMIT: usize = 40;
            let work_complete =
                facts.overview.current_complete && facts.overview.current.len() <= PANEL_WORK_LIMIT;
            let live_complete = facts.live_complete
                && facts.sessions.len().saturating_add(facts.intents.len()) <= PANEL_LIVE_LIMIT;
            let work = if include_work {
                facts
                    .overview
                    .current
                    .iter()
                    .take(PANEL_WORK_LIMIT)
                    .map(|item| {
                        json!({
                            "agent": item.lead_agent,
                            "title": item.title,
                            "state": item.state,
                            "claim_status": item.claim_status,
                            "next_action": item.next_action,
                        })
                    })
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let live = if include_work {
                let held: std::collections::BTreeSet<&str> = facts
                    .sessions
                    .iter()
                    .filter_map(|session| session.intent.as_deref())
                    .collect();
                let mut live = facts
                    .sessions
                    .iter()
                    .take(PANEL_LIVE_LIMIT)
                    .map(|session| {
                        json!({
                            "agent": session.agent,
                            "task": session.task,
                            "source": "session",
                        })
                    })
                    .collect::<Vec<_>>();
                live.extend(
                    facts
                        .intents
                        .iter()
                        .filter(|intent| !held.contains(intent.id.as_str()))
                        .take(PANEL_LIVE_LIMIT.saturating_sub(live.len()))
                        .map(|intent| {
                            json!({
                                "agent": intent.agent,
                                "task": intent.task,
                                "source": "intent",
                            })
                        }),
                );
                live
            } else {
                Vec::new()
            };
            let summary = summarize(
                repo.full_name,
                repo.repository_id,
                owner.identity.provider.clone(),
                &facts,
            );
            let attention_categories = json!(summary.attention_categories);
            let revision = if include_work && summary.needs_you > 0 {
                revision_key
                    .as_deref()
                    .map(|key| attention_revision(&facts, &summary.contested_markers, key))
            } else {
                None
            };
            rows.push((
                summary,
                work,
                live,
                work_complete,
                live_complete,
                revision,
                attention_categories,
            ));
        }
        Ok(rows)
    })
    .await?;
    // What needs a person first, then stuck work, then the newest activity.
    rows.sort_by(|(a, _, _, _, _, _, _), (b, _, _, _, _, _, _)| {
        (b.needs_you > 0)
            .cmp(&(a.needs_you > 0))
            .then((b.unfinished.stuck > 0).cmp(&(a.unfinished.stuck > 0)))
            .then(b.last_activity_at.cmp(&a.last_activity_at))
            .then(a.repository.cmp(&b.repository))
    });
    let rows = rows
        .into_iter()
        .map(
            |(
                summary,
                work,
                live,
                work_complete,
                live_complete,
                revision,
                attention_categories,
            )| {
                let mut value =
                    serde_json::to_value(summary).expect("repository summary serializes");
                if include_work {
                    value["work"] = json!(work);
                    value["work_complete"] = json!(work_complete);
                    value["live"] = json!(live);
                    value["live_complete"] = json!(live_complete);
                    value["attention_revision"] = json!(revision);
                    value["attention_categories"] = attention_categories;
                }
                value
            },
        )
        .collect::<Vec<_>>();
    Ok(Json(json!({"repositories": rows, "unchecked": unchecked,
        "complete": complete, "checked_at": unix_now()})))
}

/// A stable change marker for actionable items, without returning paths or token values.
fn attention_revision(facts: &Facts, contested_markers: &[String], key: &str) -> String {
    let mut keys = Vec::new();
    for item in &facts.overview.current {
        if matches!(item.state, WorkState::Blocked | WorkState::Handover)
            || item.claim_status != ClaimStatus::Active
        {
            keys.push(format!("work:{}", item.id));
        }
    }
    for pair in &facts.overview.overlaps {
        keys.push(format!("overlap:{}:{}", pair.left.id, pair.right.id));
    }
    // Only contested pairs and their overlapping keys affect this revision.
    for marker in contested_markers {
        keys.push(format!("intent-pair:{marker}"));
    }
    keys.sort();
    crate::hmac::hmac_sha256(key.as_bytes(), keys.join("\0").as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The candidates `owner` may read now by [`repository_role`], and how many could not be checked
/// in time or at all. A role short of read, or a repository no longer connected, is left out.
async fn readable(
    state: &HubState,
    owner: &User,
    candidates: Vec<ConnectedRepository>,
    use_role_cache: bool,
) -> (Vec<ConnectedRepository>, usize) {
    let deadline = tokio::time::Instant::now() + LIST_DEADLINE;
    let mut pending = candidates.into_iter();
    let mut checks = tokio::task::JoinSet::new();
    let (mut allowed, mut unchecked) = (Vec::new(), 0);
    let now = unix_now();
    loop {
        while checks.len() < LIST_CONCURRENCY
            && let Some(repo) = pending.next()
        {
            let cached = use_role_cache
                .then(|| cached_role(&state.login, &owner.id, &repo, now))
                .flatten();
            match cached {
                Some(true) => allowed.push(repo),
                Some(false) => {}
                None => {
                    let (state, owner) = (state.clone(), owner.clone());
                    checks.spawn(async move {
                        let role = repository_role(&state, &owner, &repo).await;
                        (repo, role)
                    });
                }
            }
        }
        match tokio::time::timeout_at(deadline, checks.join_next()).await {
            Ok(None) => break,
            Ok(Some(Ok((repo, Ok(()))))) => {
                if use_role_cache {
                    remember_role(&state.login, &owner.id, &repo, true, unix_now());
                }
                allowed.push(repo);
            }
            Ok(Some(Ok((repo, Err(Denial::Role(_)))))) => {
                if use_role_cache {
                    remember_role(&state.login, &owner.id, &repo, false, unix_now());
                }
            }
            Ok(Some(Ok((_, Err(Denial::Kind("not_connected")))))) => {}
            Ok(Some(_)) => unchecked += 1,
            Err(_) => {
                unchecked += checks.len() + pending.len();
                checks.abort_all();
                break;
            }
        }
    }
    (allowed, unchecked)
}

/// How long the All repositories list reuses a person's role answer for one repository, so a
/// granted or lost access shows there within this long. Mints and read tokens always ask.
const ROLE_CHECK_TTL_SECONDS: i64 = 90;

/// Starts with the person, so [`forget_role_checks`] finds all of theirs.
fn role_check_key(person: &str, repo: &ConnectedRepository) -> String {
    format!("{person}\n{}\n{}", repo.connection, repo.repository_id)
}

/// Whether `person` could read `repo` when last asked within the TTL, if they were.
fn cached_role(login: &Login, person: &str, repo: &ConnectedRepository, now: i64) -> Option<bool> {
    let key = role_check_key(person, repo);
    login.pending().role_checks.get(&key, now).copied()
}

/// Keeps a forge answer (readable or not) for the TTL; failures are never kept.
fn remember_role(
    login: &Login,
    person: &str,
    repo: &ConnectedRepository,
    readable: bool,
    now: i64,
) {
    let key = role_check_key(person, repo);
    let expires_at = now + ROLE_CHECK_TTL_SECONDS;
    login
        .pending()
        .role_checks
        .insert(key, expires_at, readable, now);
}

/// Forgets every role answer kept for `person`, when they sign out.
pub(super) fn forget_role_checks(login: &Login, person: &str) {
    login
        .pending()
        .role_checks
        .remove_prefixed(&format!("{person}\n"));
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

/// At most this many connected repositories are asked about one fork.
const MAX_FORK_CANDIDATES: usize = 5;

/// The connected repository `repository` was forked from, when the forge says so. Asked only
/// for an `owner/name` this Hub does not have, so a contributor working in a fork is told which
/// remote to use instead of only "not connected". A candidate parent is a connected repository
/// with the same name, and the request goes only through that repository's own connection, so
/// no other credential is used to look the name up. Never guesses: no answer is no hint.
async fn fork_parent(state: &HubState, owner: &User, repository: &str) -> Option<String> {
    let (_, name) = repository
        .split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.contains('/'))?;
    let (store, host) = (state.store.clone(), owner.identity.host.clone());
    let suffix = format!("/{}", name.to_ascii_lowercase());
    let requested = repository.to_ascii_lowercase();
    let candidates = blocking(state, move || {
        let mut found = Vec::new();
        for repo in store.repositories_on(&host, super::MAX_SIGN_IN_PROBES)? {
            let full = repo.full_name.to_ascii_lowercase();
            if full.ends_with(&suffix) && full != requested && found.len() < MAX_FORK_CANDIDATES {
                let conn = store.connection(&repo.connection)?;
                found.extend(conn.map(|conn| (conn, repo)));
            }
        }
        Ok(found)
    })
    .await
    .ok()?;
    for (conn, candidate) in candidates {
        let Ok(token) = state.sync.token(&conn, candidate.installation_id).await else {
            continue;
        };
        let reply = match state.sync.forge(&conn).ok()? {
            ForgeApi::Github(api) => api.repo_json(&token, repository).await,
            ForgeApi::Gitlab(api) => api.project_json(&token, repository).await,
        };
        let Ok(Some(reply)) = reply else {
            continue;
        };
        let parent = [
            &reply["parent"]["full_name"],
            &reply["source"]["full_name"],
            &reply["forked_from_project"]["path_with_namespace"],
        ]
        .into_iter()
        .filter_map(Value::as_str)
        .any(|parent| parent.eq_ignore_ascii_case(&candidate.full_name));
        if parent {
            return Some(candidate.full_name);
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
        GhError::Limited(_) | GhError::Failed(_) | GhError::TooLarge(_) => {
            kind("permission_unavailable")
        }
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
    // The GitHub login is cached from sign-in; one that now has another id belongs to someone
    // else, and signing in again refreshes it. No role reply at all is judged as no role, so a
    // forge that answers 404 for a non-collaborator never signs the computer out.
    let user_id = match (&role, gitlab) {
        (Some(role), true) => role["id"].as_i64(),
        (Some(role), false) => role["user"]["id"].as_i64(),
        (None, _) => Some(owner.identity.user_id),
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

    fn connected(repository_id: i64) -> ConnectedRepository {
        ConnectedRepository {
            id: format!("rep_{}", "0".repeat(32)),
            connection: "con_test".into(),
            host: "github.com".into(),
            repository_id,
            full_name: format!("acme/r{repository_id}"),
            node_id: None,
            installation_id: Some(1),
            pulls_etag: None,
        }
    }

    #[test]
    fn role_answers_are_kept_per_person_until_the_ttl_or_sign_out() {
        let login = Login::unconfigured();
        let (widgets, docs) = (connected(1), connected(2));
        assert_eq!(cached_role(&login, "usr_a", &widgets, 100), None);
        remember_role(&login, "usr_a", &widgets, true, 100);
        remember_role(&login, "usr_a", &docs, false, 100);
        assert_eq!(cached_role(&login, "usr_a", &widgets, 100), Some(true));
        assert_eq!(cached_role(&login, "usr_a", &docs, 150), Some(false));
        // Never another person's, even one whose id starts the same.
        assert_eq!(cached_role(&login, "usr_b", &widgets, 100), None);
        assert_eq!(cached_role(&login, "usr_", &widgets, 100), None);
        assert_eq!(cached_role(&login, "usr_ab", &widgets, 100), None);
        // Asked again once the TTL is over.
        let expired = 100 + ROLE_CHECK_TTL_SECONDS;
        assert_eq!(
            cached_role(&login, "usr_a", &widgets, expired - 1),
            Some(true)
        );
        assert_eq!(cached_role(&login, "usr_a", &widgets, expired), None);
        // Signing out forgets only that person's.
        remember_role(&login, "usr_ab", &widgets, true, 100);
        forget_role_checks(&login, "usr_a");
        assert_eq!(cached_role(&login, "usr_a", &docs, 100), None);
        assert_eq!(cached_role(&login, "usr_ab", &widgets, 100), Some(true));
    }
}
