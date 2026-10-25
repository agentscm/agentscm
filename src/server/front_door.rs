//! The local front door: `agentscm serve --front-door` serves the Hub page on loopback for every
//! repository connected on this computer.
//!
//! `agentscm open` (or the tray) mints a single-use code through the admin secret in the
//! discovery record and opens the page with it in the URL fragment. The page redeems the code for
//! a browser grant held only in this process's memory, so a restarted service, or any other
//! listener on the port, never honours it. Each request names its repository next to the grant.
//! Private reads (`/v1/local`) are served here; Hub reads (`/v1/hub`) are forwarded with a
//! read-only, single-repository token this process mints from the saved human session. That token
//! never reaches the browser.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use agentscm::Result;
use agentscm::repo_explorer::Explorer;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::AppState;
use crate::client_config;
use crate::hub_read_token::{self as hub_open, OpenError};
use crate::local_service::{random_hex, valid_hex};

/// Long enough for a browser to start and load the page; short enough that a copied link is dead.
const CODE_TTL_SECONDS: i64 = 120;
/// A grant lasts a week at most, and never longer than this process.
const GRANT_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
const MAX_PENDING: usize = 8;
const MAX_GRANTS: usize = 32;
/// Re-mint a cached Hub token this long before it expires.
const TOKEN_RENEW_SECONDS: i64 = 120;
const MAX_HUB_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// The browser credential: `local:<grant>:<owner/name>[@<hub>]`.
const CREDENTIAL_PREFIX: &str = "local:";
/// Hub areas the page reaches with its own cookie session or that mint credentials; never proxied.
const UNPROXIED: [&str; 4] = [
    "/v1/hub/workspace",
    "/v1/hub/login",
    "/v1/hub/tokens",
    "/v1/hub/token",
];

pub(super) struct FrontDoor {
    pub instance: String,
    pub computer: String,
    admin: String,
    origin: OnceLock<String>,
    codes: Mutex<Codes>,
    explorers: Mutex<HashMap<String, Arc<Explorer>>>,
    tokens: tokio::sync::Mutex<HashMap<(String, String), CachedToken>>,
    client: reqwest::Client,
}

#[derive(Default)]
struct Codes {
    pending: Vec<(String, i64)>,
    grants: Vec<Grant>,
}

struct Grant {
    id: String,
    secret_sha256: String,
    expires_at: i64,
}

struct CachedToken {
    id: String,
    secret: String,
    expires_at: i64,
    human_session: String,
}

/// The connected checkout a request reads, chosen by the credential and checked against this
/// computer's connections on every request.
#[derive(Clone, Debug)]
pub(super) struct Selected {
    pub repository: String,
    pub hub: String,
    pub root: PathBuf,
    origin_host: String,
    grant: String,
}

fn now() -> i64 {
    agentscm::hub::store::unix_now()
}

fn digest(secret: &str) -> String {
    format!("{:x}", Sha256::digest(secret.as_bytes()))
}

fn same(left: &str, right: &str) -> bool {
    left.len() == right.len()
        && left
            .bytes()
            .zip(right.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Every current connection on this computer, one per repository and Hub (the first checkout
/// wins). The same repository can be connected to different Hubs in different checkouts.
pub(super) fn connected() -> Vec<client_config::Connection> {
    let mut seen = std::collections::BTreeSet::new();
    client_config::load()
        .unwrap_or_default()
        .into_iter()
        .filter(client_config::is_current)
        .filter(|connection| {
            seen.insert((connection.repo.to_ascii_lowercase(), connection.hub.clone()))
        })
        .collect()
}

impl FrontDoor {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            instance: random_hex(16)?,
            computer: crate::local_service::computer_name(),
            admin: random_hex(32)?,
            origin: OnceLock::new(),
            codes: Mutex::new(Codes::default()),
            explorers: Mutex::new(HashMap::new()),
            tokens: tokio::sync::Mutex::new(HashMap::new()),
            client: agentscm::hub::forge_http::client()
                .map_err(|_| agentscm::Error::Invalid("HTTP client unavailable".into()))?,
        })
    }

    pub(super) fn admin_secret(&self) -> &str {
        &self.admin
    }

    /// The page's own origin, fixed once the listener is bound.
    pub(super) fn set_origin(&self, origin: String) {
        let _ = self.origin.set(origin);
    }

    fn own_origin(&self) -> &str {
        self.origin.get().map_or("", String::as_str)
    }

    fn codes(&self) -> std::sync::MutexGuard<'_, Codes> {
        let mut codes = self
            .codes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let now = now();
        codes.pending.retain(|(_, expires_at)| *expires_at > now);
        codes.grants.retain(|grant| grant.expires_at > now);
        codes
    }

    fn mint_code(&self) -> Result<(String, i64)> {
        let code = random_hex(32)?;
        let expires_at = now() + CODE_TTL_SECONDS;
        let mut codes = self.codes();
        if codes.pending.len() >= MAX_PENDING {
            codes.pending.remove(0);
        }
        codes.pending.push((digest(&code), expires_at));
        Ok((code, expires_at))
    }

    /// Consume the code whatever the outcome, so a replayed code is dead after one try.
    fn redeem(&self, code: &str) -> Result<Option<(String, i64)>> {
        if !valid_hex(code, 64) {
            return Ok(None);
        }
        let code = digest(code);
        let mut codes = self.codes();
        let Some(index) = codes
            .pending
            .iter()
            .position(|(stored, _)| same(stored, &code))
        else {
            return Ok(None);
        };
        codes.pending.remove(index);
        let secret = random_hex(32)?;
        let expires_at = now() + GRANT_TTL_SECONDS;
        if codes.grants.len() >= MAX_GRANTS {
            codes.grants.remove(0);
        }
        codes.grants.push(Grant {
            id: random_hex(8)?,
            secret_sha256: digest(&secret),
            expires_at,
        });
        Ok(Some((secret, expires_at)))
    }

    fn grant_id(&self, secret: &str) -> Option<(String, i64)> {
        if !valid_hex(secret, 64) {
            return None;
        }
        let presented = digest(secret);
        self.codes()
            .grants
            .iter()
            .find(|grant| same(&grant.secret_sha256, &presented))
            .map(|grant| (grant.id.clone(), grant.expires_at))
    }

    fn revoke_grant(&self, id: &str) {
        self.codes().grants.retain(|grant| grant.id != id);
    }

    /// The selection named by a `local:<grant>:<owner/name>[@<hub>]` credential, if the grant is
    /// live and that repository (on that Hub, when named) is connected on this computer now.
    fn select(&self, headers: &HeaderMap) -> Option<Selected> {
        let credential = bearer(headers)?.strip_prefix(CREDENTIAL_PREFIX)?;
        let (grant, selector) = credential.split_once(':')?;
        self.grant_id(grant)?;
        let (repository, hub) = match selector.split_once('@') {
            Some((repository, hub)) => (repository, Some(hub)),
            None => (selector, None),
        };
        let connection = connected().into_iter().find(|connection| {
            connection.repo.eq_ignore_ascii_case(repository)
                && hub.is_none_or(|hub| hub == connection.hub)
        })?;
        Some(Selected {
            repository: connection.repo,
            hub: connection.hub,
            root: connection.root,
            origin_host: connection.origin_host,
            grant: grant.to_owned(),
        })
    }

    /// The explorer for a connected checkout, opened on first use. Capture is included only when
    /// change capture is already bound to that exact checkout; connecting never enables it.
    pub(super) fn explorer(&self, selected: &Selected) -> Result<Arc<Explorer>> {
        let key = selected.root.display().to_string();
        if let Some(explorer) = self.lock_explorers().get(&key) {
            return Ok(explorer.clone());
        }
        let spool = crate::change_capture_client::spool_for_checkout(
            &selected.root,
            &selected.repository,
            &selected.hub,
        )?;
        let explorer = super::explorer::open_checkout(&selected.root, &selected.repository, spool)?;
        self.lock_explorers().insert(key, explorer.clone());
        Ok(explorer)
    }

    /// Forget a cached explorer after a failed read, so a moved or re-cloned checkout reopens.
    pub(super) fn forget_explorer(&self, selected: &Selected) {
        self.lock_explorers()
            .remove(&selected.root.display().to_string());
    }

    fn lock_explorers(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Explorer>>> {
        self.explorers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A read-only Hub token for the selected repository, minted from the saved human session and
    /// cached until shortly before it expires.
    async fn hub_token(
        &self,
        selected: &Selected,
        renew: bool,
    ) -> std::result::Result<String, OpenError> {
        let key = (
            selected.hub.clone(),
            selected.repository.to_ascii_lowercase(),
        );
        let mut tokens = self.tokens.lock().await;
        if !renew
            && let Some(cached) = tokens.get(&key)
            && cached.expires_at > now() + TOKEN_RENEW_SECONDS
        {
            return Ok(cached.secret.clone());
        }
        let (hub, human_session) = hub_open::load_exact_session(&selected.hub)?;
        let provider = hub_open::expected_provider(
            &selected.origin_host,
            &crate::client_status::HubCheck::Skipped,
        );
        let minted = hub_open::mint_and_confirm(
            &self.client,
            &hub,
            &human_session,
            &selected.repository,
            &selected.origin_host,
            provider.as_deref(),
            now(),
        )
        .await?;
        let replaced = tokens.insert(
            key,
            CachedToken {
                id: minted.id.clone(),
                secret: minted.secret.clone(),
                expires_at: minted.expires_at,
                human_session,
            },
        );
        drop(tokens);
        if let Some(old) = replaced {
            hub_open::revoke_token(
                &self.client,
                &hub,
                Some(&old.secret),
                Some(&old.id),
                &old.human_session,
            )
            .await;
        }
        Ok(minted.secret)
    }

    /// Revoke every Hub token this process minted.
    pub(super) async fn revoke_all(&self) {
        let tokens = std::mem::take(&mut *self.tokens.lock().await);
        for ((hub, _), token) in tokens {
            hub_open::revoke_token(
                &self.client,
                &hub,
                Some(&token.secret),
                Some(&token.id),
                &token.human_session,
            )
            .await;
        }
    }
}

fn refuse(status: StatusCode, error: &str, message: &str) -> Response {
    let mut response = (status, Json(json!({"error": error, "message": message}))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

const RECONNECT: &str = "This browser is not connected to this computer. Run `agentscm open` in your checkout, or choose Open Hub in the AgentSCM menu bar app.";

pub(super) fn router(state: AppState, front_door: Arc<FrontDoor>) -> Router {
    let guarded = Router::new()
        .route("/v1/hub/{*path}", get(proxy))
        .route("/v1/local/repositories", get(repositories))
        .route("/v1/local/session", delete(disconnect))
        .with_state(front_door.clone())
        .merge(super::explorer::routes().with_state(state.clone()))
        .layer(middleware::from_fn_with_state(front_door.clone(), guard));
    let open = Router::new()
        .route("/v1/local/pairings", post(mint))
        .route("/v1/local/pairings/redeem", post(redeem))
        .route("/v1/local/identity", get(identity))
        .with_state(front_door.clone())
        .layer(middleware::from_fn_with_state(front_door, same_origin_only));
    Router::new()
        .route("/health", get(super::health))
        .merge(agentscm::hub::ui::routes())
        .merge(guarded)
        .merge(open)
}

/// Loopback Host (DNS rebinding) and same-origin checks shared by every API route.
fn origin_refusal(front_door: &FrontDoor, headers: &HeaderMap) -> Option<Response> {
    if !super::loopback_host(headers) {
        return Some(refuse(
            StatusCode::MISDIRECTED_REQUEST,
            "misdirected",
            "local reads require a loopback Host (127.0.0.1 or [::1])",
        ));
    }
    let origin = headers.get(header::ORIGIN)?;
    (origin.as_bytes() != front_door.own_origin().as_bytes()).then(|| {
        refuse(
            StatusCode::FORBIDDEN,
            "origin_refused",
            "only this computer's own AgentSCM page may read here",
        )
    })
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn same_origin_only(
    State(front_door): State<Arc<FrontDoor>>,
    request: Request,
    next: Next,
) -> Response {
    no_store(match origin_refusal(&front_door, request.headers()) {
        Some(refusal) => refusal,
        None => next.run(request).await,
    })
}

async fn guard(
    State(front_door): State<Arc<FrontDoor>>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(refusal) = origin_refusal(&front_door, request.headers()) {
        return no_store(refusal);
    }
    let Some(selected) = front_door.select(request.headers()) else {
        return no_store(refuse(StatusCode::UNAUTHORIZED, "not_connected", RECONNECT));
    };
    request.extensions_mut().insert(selected);
    no_store(next.run(request).await)
}

async fn mint(State(front_door): State<Arc<FrontDoor>>, headers: HeaderMap) -> Response {
    // The admin secret is for `agentscm open` and the tray, never a browser page.
    let admin =
        bearer(&headers).is_some_and(|presented| same(presented, front_door.admin_secret()));
    if !admin || headers.contains_key(header::ORIGIN) {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "minting a code requires this service's admin secret",
        );
    }
    match front_door.mint_code() {
        Ok((code, expires_at)) => (
            StatusCode::CREATED,
            Json(json!({"code": code, "expires_at": expires_at, "instance": front_door.instance})),
        )
            .into_response(),
        Err(_) => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "a connection code could not be created",
        ),
    }
}

async fn redeem(State(front_door): State<Arc<FrontDoor>>, headers: HeaderMap) -> Response {
    // A browser always sends Origin on this POST; the page must be this service's own.
    let from_page = headers
        .get(header::ORIGIN)
        .is_some_and(|origin| origin.as_bytes() == front_door.own_origin().as_bytes());
    let Some(code) = bearer(&headers).filter(|_| from_page) else {
        return refuse(
            StatusCode::UNAUTHORIZED,
            "code_invalid",
            "this connection link is not valid",
        );
    };
    match front_door.redeem(code) {
        Ok(Some((grant, expires_at))) => Json(json!({
            "version": 1,
            "grant": grant,
            "expires_at": expires_at,
            "instance": front_door.instance,
            "computer": front_door.computer,
        }))
        .into_response(),
        Ok(None) => refuse(
            StatusCode::UNAUTHORIZED,
            "code_invalid",
            "this connection link was already used or has expired; run `agentscm open` again",
        ),
        Err(_) => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            "the connection could not be completed",
        ),
    }
}

/// Without credentials, only the instance, so `agentscm open` and the tray can confirm which
/// process listens before sending their secret. With a browser credential, what the page shows.
async fn identity(State(front_door): State<Arc<FrontDoor>>, headers: HeaderMap) -> Response {
    if !headers.contains_key(header::AUTHORIZATION) {
        return Json(json!({"version": 1, "instance": front_door.instance})).into_response();
    }
    let Some(selected) = front_door.select(&headers) else {
        return refuse(StatusCode::UNAUTHORIZED, "not_connected", RECONNECT);
    };
    let expires_at = front_door.grant_id(&selected.grant).map(|(_, at)| at);
    let signed_in = crate::hub_cli::has_session(Some(&selected.hub)).unwrap_or(false);
    Json(json!({
        "version": 1,
        "instance": front_door.instance,
        "computer": front_door.computer,
        "repository": selected.repository,
        "hub": {"url": selected.hub, "signed_in": signed_in},
        "expires_at": expires_at,
    }))
    .into_response()
}

async fn repositories() -> Response {
    let repositories: Vec<_> = connected()
        .into_iter()
        .map(|connection| {
            let folder = connection
                .root
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            json!({"repository": connection.repo, "hub": connection.hub, "folder": folder})
        })
        .collect();
    Json(json!({"version": 1, "repositories": repositories})).into_response()
}

async fn disconnect(
    State(front_door): State<Arc<FrontDoor>>,
    axum::Extension(selected): axum::Extension<Selected>,
) -> Response {
    if let Some((id, _)) = front_door.grant_id(&selected.grant) {
        front_door.revoke_grant(&id);
    }
    StatusCode::NO_CONTENT.into_response()
}

/// Forward a Hub read for the selected repository with this process's read-only token.
async fn proxy(
    State(front_door): State<Arc<FrontDoor>>,
    axum::Extension(selected): axum::Extension<Selected>,
    uri: axum::http::Uri,
) -> Response {
    let target = uri
        .path_and_query()
        .map_or(uri.path(), axum::http::uri::PathAndQuery::as_str);
    if UNPROXIED
        .iter()
        .any(|prefix| uri.path() == *prefix || uri.path().starts_with(&format!("{prefix}/")))
    {
        return refuse(
            StatusCode::NOT_FOUND,
            "not_proxied",
            "this Hub area opens on the Hub itself",
        );
    }
    for renew in [false, true] {
        let token = match front_door.hub_token(&selected, renew).await {
            Ok(token) => token,
            Err(error) => return hub_refusal(error, &selected.hub),
        };
        let reply = front_door
            .client
            .get(format!("{}{target}", selected.hub))
            .bearer_auth(&token)
            .send()
            .await;
        match reply {
            // A revoked or expired cached token: mint once more before reporting it.
            Ok(reply) if reply.status() == reqwest::StatusCode::UNAUTHORIZED && !renew => continue,
            Ok(reply) => return relay(reply).await,
            Err(_) => break,
        }
    }
    refuse(
        StatusCode::BAD_GATEWAY,
        "hub_unavailable",
        "the Hub did not answer; try again",
    )
}

fn hub_refusal(error: OpenError, hub: &str) -> Response {
    match error {
        OpenError::MissingSignIn | OpenError::RejectedSignIn => refuse(
            StatusCode::SERVICE_UNAVAILABLE,
            "hub_sign_in_required",
            &format!(
                "This computer is not signed in to {hub}. Run `agentscm hub login --hub {hub}`, then reload."
            ),
        ),
        OpenError::RepositoryNotConnected => refuse(
            StatusCode::FORBIDDEN,
            "not_connected_to_hub",
            "this repository is not connected to its Hub; ask the Hub's operator to connect it",
        ),
        OpenError::AccessDenied => refuse(
            StatusCode::FORBIDDEN,
            "access_denied",
            "your Hub sign-in cannot read this repository",
        ),
        _ => refuse(
            StatusCode::BAD_GATEWAY,
            "hub_unavailable",
            "the Hub is unavailable; try again",
        ),
    }
}

async fn relay(mut reply: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(reply.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = reply.headers().get(reqwest::header::CONTENT_TYPE).cloned();
    let mut body = Vec::new();
    loop {
        match reply.chunk().await {
            Ok(Some(chunk)) if body.len() + chunk.len() <= MAX_HUB_RESPONSE_BYTES => {
                body.extend_from_slice(&chunk);
            }
            Ok(Some(_)) => {
                return refuse(
                    StatusCode::BAD_GATEWAY,
                    "hub_response_too_large",
                    "the Hub's reply exceeds this computer's limit",
                );
            }
            Ok(None) => break,
            Err(_) => {
                return refuse(
                    StatusCode::BAD_GATEWAY,
                    "hub_unavailable",
                    "the Hub's reply was interrupted; try again",
                );
            }
        }
    }
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    if let Some(content_type) = content_type
        && let Ok(value) = HeaderValue::from_bytes(content_type.as_bytes())
    {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_redeems_once_and_grants_live_only_in_this_process() {
        let front_door = FrontDoor::new().unwrap();
        let (code, _) = front_door.mint_code().unwrap();
        let (grant, _) = front_door.redeem(&code).unwrap().unwrap();
        assert!(
            front_door.redeem(&code).unwrap().is_none(),
            "replay is refused"
        );
        assert!(front_door.grant_id(&grant).is_some());
        let other = FrontDoor::new().unwrap();
        assert!(
            other.grant_id(&grant).is_none(),
            "another process never honours it"
        );
    }

    #[test]
    fn expired_codes_and_grants_are_dropped_and_revocation_sticks() {
        let front_door = FrontDoor::new().unwrap();
        let (code, _) = front_door.mint_code().unwrap();
        front_door.codes.lock().unwrap().pending[0].1 = now() - 1;
        assert!(front_door.redeem(&code).unwrap().is_none());

        let (code, _) = front_door.mint_code().unwrap();
        let (grant, _) = front_door.redeem(&code).unwrap().unwrap();
        let (id, _) = front_door.grant_id(&grant).unwrap();
        front_door.revoke_grant(&id);
        assert!(front_door.grant_id(&grant).is_none());

        let (code, _) = front_door.mint_code().unwrap();
        let (grant, _) = front_door.redeem(&code).unwrap().unwrap();
        front_door.codes.lock().unwrap().grants[0].expires_at = now() - 1;
        assert!(front_door.grant_id(&grant).is_none());
    }

    #[test]
    fn credentials_need_the_prefix_a_live_grant_and_a_connected_repository() {
        let front_door = FrontDoor::new().unwrap();
        let (code, _) = front_door.mint_code().unwrap();
        let (grant, _) = front_door.redeem(&code).unwrap().unwrap();
        let with = |value: String| {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&value).unwrap(),
            );
            headers
        };
        // No connections exist in the test environment, so even a live grant selects nothing.
        assert!(
            front_door
                .select(&with(format!("Bearer local:{grant}:owner/missing")))
                .is_none()
        );
        assert!(
            front_door
                .select(&with(format!("Bearer {grant}")))
                .is_none()
        );
        assert!(
            front_door
                .select(&with("Bearer local:nothex:owner/name".into()))
                .is_none()
        );
    }
}
