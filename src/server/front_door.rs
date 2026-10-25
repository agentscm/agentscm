//! The local front door: `agentscm serve --front-door` serves the Hub page on loopback for every
//! repository connected on this computer.
//!
//! `agentscm open` (or the tray) mints a single-use code through the admin secret in the
//! discovery record and hands it to the browser in the URL fragment. The page redeems the code
//! for a browser grant held only in this process's memory, so a restarted service, or any other
//! listener on the port, never honours it. Each request names its repository next to the grant.
//! Private reads (`/v1/local`) are served here; Hub reads (`/v1/hub`) are forwarded with a
//! read-only, single-repository token this process mints from the saved human session. That token
//! never reaches the browser.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use agentscm::Result;
use agentscm::change_record::CaptureModeV1;
use agentscm::repo_explorer::Explorer;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::AppState;
use crate::client_config::{self, Connection};
use crate::hub_read_token::{self, OpenError};
use crate::local_service::{random_hex, valid_hex};

/// Long enough for a browser to start and load the page; short enough that a copied link is dead.
const CODE_TTL_SECONDS: i64 = 120;
/// A grant lasts a week at most, and never longer than this process.
const GRANT_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
const MAX_PENDING: usize = 8;
const MAX_GRANTS: usize = 32;
/// Re-mint a cached Hub token this long before it expires.
const TOKEN_RENEW_SECONDS: i64 = 120;
/// A replaced Hub token is revoked after reads already using it have finished.
const RETIRED_TOKEN_GRACE: Duration = Duration::from_secs(60);
const MAX_HUB_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Hub reads in flight at once, which bounds the memory replies can buffer.
const MAX_HUB_READS: usize = 8;
/// How long the list of current connections is reused before Git is asked again.
const CONNECTIONS_TTL: Duration = Duration::from_secs(5);
const SHUTDOWN_REVOKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The most one mint (session check, forge check, mint and confirmation) may take, so a slow
/// Hub cannot hold a repository's token for longer.
const MINT_TIMEOUT: Duration = Duration::from_secs(20);
/// Names the tokens this service mints in the Hub's token list.
const TOKEN_NAME: &str = "agentscm-local-page";
/// The browser credential: `local:<grant>:<owner/name>[@<hub>]`.
const CREDENTIAL_PREFIX: &str = "local:";
/// The Hub reads the page makes. Anything else, including sign-in, workspace and token areas,
/// stays on the Hub itself.
const PROXIED: [&str; 8] = [
    "/v1/hub/whoami",
    "/v1/hub/health",
    "/v1/hub/onboarding",
    "/v1/hub/view-settings",
    "/v1/hub/skill-workflows",
    "/v1/hub/skills",
    "/v1/hub/standup",
    "/v1/hub/repos",
];

type TokenKey = (String, String, String);
type TokenSlot = Arc<tokio::sync::Mutex<Option<CachedToken>>>;

pub(super) struct FrontDoor {
    pub instance: String,
    pub computer: String,
    admin: String,
    origin: OnceLock<String>,
    codes: Mutex<Codes>,
    connections: tokio::sync::Mutex<Option<(Instant, Arc<Vec<Connection>>)>>,
    explorers: Mutex<HashMap<ExplorerKey, Arc<Explorer>>>,
    tokens: Mutex<HashMap<TokenKey, TokenSlot>>,
    /// Replaced tokens awaiting their delayed revocation; shutdown revokes whatever is left.
    retired: Arc<Mutex<Vec<CachedToken>>>,
    hub_reads: tokio::sync::Semaphore,
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

#[derive(Clone)]
struct CachedToken {
    hub: String,
    id: String,
    secret: String,
    expires_at: i64,
    human_session: String,
}

/// A cached explorer is reused only while the checkout, its connection and its capture binding
/// are unchanged.
type ExplorerKey = (PathBuf, String, String, Option<PathBuf>);

/// A live browser grant, before any repository is chosen.
#[derive(Clone, Debug)]
pub(super) struct Granted {
    grant: String,
    expires_at: i64,
}

/// The connected checkout a request reads, chosen by the credential and checked against this
/// computer's connections.
#[derive(Clone, Debug)]
pub(super) struct Selected {
    pub repository: String,
    pub hub: String,
    pub root: PathBuf,
    origin_host: String,
    connection: Connection,
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

/// The grant and selector of a `local:<grant>:<selector>` credential.
fn credential(headers: &HeaderMap) -> Option<(&str, &str)> {
    bearer(headers)?
        .strip_prefix(CREDENTIAL_PREFIX)?
        .split_once(':')
}

/// Every current checkout connection on this computer.
fn all_connections() -> Vec<Connection> {
    client_config::load()
        .unwrap_or_default()
        .into_iter()
        .filter(client_config::is_current)
        .collect()
}

/// Every current connection on this computer, one per repository and Hub (the first checkout
/// wins), which is what a selector can name. The same repository can be connected to different
/// Hubs in different checkouts.
fn current_connections() -> Vec<Connection> {
    let mut seen = std::collections::BTreeSet::new();
    all_connections()
        .into_iter()
        .filter(|connection| {
            seen.insert((connection.repo.to_ascii_lowercase(), connection.hub.clone()))
        })
        .collect()
}

/// Whether a Hub path is one the page reads, in its plain form: encoded dots or backslashes, and
/// empty, `.` or `..` segments, are refused before the URL is ever normalized. An encoded slash
/// stays (the page sends `owner%2Fname` as one segment) because normalization never decodes it.
fn proxied(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if ["%2e", "%5c", "\\"].iter().any(|bad| lower.contains(bad))
        || path
            .split('/')
            .skip(1)
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return false;
    }
    PROXIED
        .iter()
        .any(|allowed| path == *allowed || path.starts_with(&format!("{allowed}/")))
}

/// The selected Hub's URL for an allowlisted read. Scheme, host and port always come from the
/// saved Hub; the request supplies only the path below it and the query.
fn hub_read_url(hub: &str, path: &str, query: Option<&str>) -> Option<reqwest::Url> {
    let mut url = reqwest::Url::parse(hub).ok()?;
    let joined = format!("{}{path}", url.path().trim_end_matches('/'));
    url.set_path(&joined);
    url.set_query(query);
    url.set_fragment(None);
    Some(url)
}

impl FrontDoor {
    pub(super) fn new() -> Result<Self> {
        Ok(Self {
            instance: random_hex(16)?,
            computer: crate::local_service::computer_name(),
            admin: random_hex(32)?,
            origin: OnceLock::new(),
            codes: Mutex::new(Codes::default()),
            connections: tokio::sync::Mutex::new(None),
            explorers: Mutex::new(HashMap::new()),
            tokens: Mutex::new(HashMap::new()),
            retired: Arc::new(Mutex::new(Vec::new())),
            hub_reads: tokio::sync::Semaphore::new(MAX_HUB_READS),
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

    /// The live grant a request presents, whichever repository it names.
    fn granted(&self, headers: &HeaderMap) -> Option<Granted> {
        let (grant, _) = credential(headers)?;
        let (_, expires_at) = self.grant_id(grant)?;
        Some(Granted {
            grant: grant.to_owned(),
            expires_at,
        })
    }

    /// This computer's current connections, rechecked with Git at most every few seconds unless
    /// `fresh` asks for a recheck now.
    async fn connected(&self, fresh: bool) -> Arc<Vec<Connection>> {
        let asked = Instant::now();
        let mut cached = self.connections.lock().await;
        // A forced recheck reuses a list checked after it was asked for, so a burst of requests
        // waiting on this lock checks Git once without missing a checkout connected just before.
        if let Some((at, connections)) = cached.as_ref()
            && if fresh {
                *at >= asked
            } else {
                at.elapsed() < CONNECTIONS_TTL
            }
        {
            return connections.clone();
        }
        // Stamped with when the check began: it reflects nothing connected after that.
        let checked = Instant::now();
        let connections = Arc::new(
            tokio::task::spawn_blocking(current_connections)
                .await
                .unwrap_or_default(),
        );
        *cached = Some((checked, connections.clone()));
        connections
    }

    /// The connected checkout a credential's `<owner/name>[@<hub>]` selector names. A miss is
    /// rechecked once, so a checkout connected moments ago is found at once.
    async fn select(&self, headers: &HeaderMap) -> Option<Selected> {
        let (_, selector) = credential(headers)?;
        let (repository, hub) = match selector.split_once('@') {
            Some((repository, hub)) => (repository, Some(hub)),
            None => (selector, None),
        };
        for fresh in [false, true] {
            let found = self.connected(fresh).await.iter().find_map(|connection| {
                (connection.repo.eq_ignore_ascii_case(repository)
                    && hub.is_none_or(|hub| hub == connection.hub))
                .then(|| Selected {
                    repository: connection.repo.clone(),
                    hub: connection.hub.clone(),
                    root: connection.root.clone(),
                    origin_host: connection.origin_host.clone(),
                    connection: connection.clone(),
                })
            });
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// The explorer for a connected checkout. Capture is included only when change capture is
    /// already bound to that exact checkout; connecting never enables it.
    pub(super) fn explorer(&self, selected: &Selected) -> Result<Arc<Explorer>> {
        let spool = crate::change_capture_client::spool_for_checkout(
            &selected.root,
            &selected.repository,
            &selected.hub,
        )?;
        let key = (
            selected.root.clone(),
            selected.repository.to_ascii_lowercase(),
            selected.hub.clone(),
            spool.clone(),
        );
        if let Some(explorer) = self.lock_explorers().get(&key) {
            return Ok(explorer.clone());
        }
        let explorer = super::explorer::open_checkout(&selected.root, &selected.repository, spool)?;
        let mut explorers = self.lock_explorers();
        // A changed binding replaces whatever was cached for this checkout.
        explorers.retain(|(root, ..), _| root != &selected.root);
        explorers.insert(key, explorer.clone());
        Ok(explorer)
    }

    /// Forget the cached explorers of a checkout after a failed read, so a moved or re-cloned
    /// checkout reopens.
    pub(super) fn forget_explorer(&self, selected: &Selected) {
        self.lock_explorers()
            .retain(|(root, ..), _| root != &selected.root);
    }

    fn lock_explorers(&self) -> std::sync::MutexGuard<'_, HashMap<ExplorerKey, Arc<Explorer>>> {
        self.explorers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn token_slot(&self, key: TokenKey) -> TokenSlot {
        self.tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(key)
            .or_default()
            .clone()
    }

    /// A read-only Hub token for the selected repository, minted from the saved human session and
    /// cached until shortly before it expires. `failed` is a token the Hub just refused: it is
    /// replaced only if it is still the cached one, so concurrent refusals mint once.
    async fn hub_token(
        &self,
        selected: &Selected,
        failed: Option<&str>,
    ) -> std::result::Result<String, OpenError> {
        let slot = self.token_slot((
            selected.hub.clone(),
            selected.repository.to_ascii_lowercase(),
            selected.origin_host.clone(),
        ));
        let mut cached = slot.lock().await;
        let session = hub_read_token::load_exact_session(&selected.hub);
        // A token minted from a sign-in that was since removed or replaced is revoked now, on
        // its next use, rather than living until it expires.
        if cached.as_ref().is_some_and(
            |token| !matches!(&session, Ok((_, current)) if *current == token.human_session),
        ) && let Some(stale) = cached.take()
        {
            let client = self.client.clone();
            tokio::spawn(async move { revoke(&client, &stale).await });
        }
        if let Some(token) = cached.as_ref()
            && failed.is_none_or(|failed| failed != token.secret)
            && token.expires_at > now() + TOKEN_RENEW_SECONDS
        {
            return Ok(token.secret.clone());
        }
        let (hub, human_session) = session?;
        let minted = tokio::time::timeout(MINT_TIMEOUT, async {
            let provider = self.forge_provider(selected).await?;
            hub_read_token::mint_and_confirm(
                &self.client,
                &hub,
                &human_session,
                &hub_read_token::TokenRequest {
                    repository: &selected.repository,
                    origin_host: &selected.origin_host,
                    provider: Some(&provider),
                    name: TOKEN_NAME,
                },
                now(),
            )
            .await
        })
        .await
        .map_err(|_| OpenError::Unavailable)??;
        let retired = cached.replace(CachedToken {
            hub,
            id: minted.id.clone(),
            secret: minted.secret.clone(),
            expires_at: minted.expires_at,
            human_session,
        });
        if let Some(retired) = retired {
            let id = retired.id.clone();
            self.lock_retired().push(retired);
            let (client, list) = (self.client.clone(), self.retired.clone());
            tokio::spawn(async move {
                tokio::time::sleep(RETIRED_TOKEN_GRACE).await;
                let token = {
                    let mut list = list
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    list.iter()
                        .position(|token| token.id == id)
                        .map(|at| list.remove(at))
                };
                if let Some(token) = token {
                    revoke(&client, &token).await;
                }
            });
        }
        Ok(minted.secret)
    }

    /// The forge a token for this checkout must belong to: known for github.com and gitlab.com,
    /// otherwise what the Hub reports for the checkout's own agent token, as the tray checks it. A
    /// self-hosted forge the Hub cannot vouch for gets no token.
    async fn forge_provider(&self, selected: &Selected) -> std::result::Result<String, OpenError> {
        let check = match selected.origin_host.as_str() {
            "github.com" | "gitlab.com" => crate::client_status::HubCheck::Skipped,
            _ => {
                let agent_token = crate::client_cli::read_token(&selected.connection.token_file)
                    .map_err(|_| OpenError::InvalidConnection)?;
                crate::client_status::check_hub(&selected.connection, &agent_token, None)
                    .await
                    .0
            }
        };
        hub_read_token::expected_provider(&selected.origin_host, &check)
            .ok_or(OpenError::Unavailable)
    }

    fn lock_retired(&self) -> std::sync::MutexGuard<'_, Vec<CachedToken>> {
        self.retired
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Revoke every Hub token this process holds, current and retired, together and within one
    /// bound that also covers waiting for a mint in progress.
    pub(super) async fn revoke_all(&self) {
        let slots: Vec<TokenSlot> = self
            .tokens
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain()
            .map(|(_, slot)| slot)
            .collect();
        let retired: Vec<CachedToken> = self.lock_retired().drain(..).collect();
        let client = self.client.clone();
        let revoke_everything = async move {
            let mut revocations = tokio::task::JoinSet::new();
            let mut tokens = retired;
            for slot in slots {
                tokens.extend(slot.lock().await.take());
            }
            for token in tokens {
                let client = client.clone();
                revocations.spawn(async move { revoke(&client, &token).await });
            }
            revocations.join_all().await;
        };
        let _ = tokio::time::timeout(SHUTDOWN_REVOKE_TIMEOUT, revoke_everything).await;
    }
}

async fn revoke(client: &reqwest::Client, token: &CachedToken) {
    hub_read_token::revoke_token(
        client,
        &token.hub,
        Some(&token.secret),
        Some(&token.id),
        &token.human_session,
    )
    .await;
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
const REPOSITORY_GONE: &str = "That repository is no longer connected on this computer. Choose another one, or run `agentscm open` in its checkout.";

pub(super) fn router(state: AppState, front_door: Arc<FrontDoor>) -> Router {
    let selected = Router::new()
        .route("/v1/hub/{*path}", get(proxy))
        .with_state(front_door.clone())
        .merge(super::explorer::routes().with_state(state.clone()))
        .layer(middleware::from_fn_with_state(
            front_door.clone(),
            require_repository,
        ));
    let granted = Router::new()
        .route("/v1/local/repositories", get(repositories))
        .route("/v1/local/session", delete(disconnect))
        .route(
            "/v1/local/capture",
            get(capture_settings)
                .post(capture_change)
                .layer(DefaultBodyLimit::max(1024)),
        )
        .with_state(front_door.clone())
        .merge(selected)
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
        .merge(granted)
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

/// A live grant from this page: every route but pairing and identity needs one.
async fn guard(
    State(front_door): State<Arc<FrontDoor>>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(refusal) = origin_refusal(&front_door, request.headers()) {
        return no_store(refusal);
    }
    let Some(granted) = front_door.granted(request.headers()) else {
        return no_store(refuse(StatusCode::UNAUTHORIZED, "not_connected", RECONNECT));
    };
    request.extensions_mut().insert(granted);
    no_store(next.run(request).await)
}

/// A connected repository: Hub and explorer reads need one. A repository that is no longer
/// connected is not a reason to end the browser's connection.
async fn require_repository(
    State(front_door): State<Arc<FrontDoor>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(selected) = front_door.select(request.headers()).await else {
        return refuse(
            StatusCode::CONFLICT,
            "repository_unavailable",
            REPOSITORY_GONE,
        );
    };
    request.extensions_mut().insert(selected);
    next.run(request).await
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
/// process listens before sending their secret. With a browser credential, what the page shows;
/// `repository` is null when the selected one is no longer connected.
async fn identity(State(front_door): State<Arc<FrontDoor>>, headers: HeaderMap) -> Response {
    if !headers.contains_key(header::AUTHORIZATION) {
        return Json(json!({"version": 1, "instance": front_door.instance})).into_response();
    }
    let Some(granted) = front_door.granted(&headers) else {
        return refuse(StatusCode::UNAUTHORIZED, "not_connected", RECONNECT);
    };
    let selected = front_door.select(&headers).await;
    let hub = match &selected {
        Some(selected) => {
            let url = selected.hub.clone();
            let signed_in =
                tokio::task::spawn_blocking(move || crate::hub_cli::has_session(Some(&url)))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or(false);
            json!({"url": selected.hub, "signed_in": signed_in})
        }
        None => serde_json::Value::Null,
    };
    Json(json!({
        "version": 1,
        "instance": front_door.instance,
        "computer": front_door.computer,
        "repository": selected.as_ref().map(|selected| &selected.repository),
        "hub": hub,
        "expires_at": granted.expires_at,
    }))
    .into_response()
}

async fn repositories(State(front_door): State<Arc<FrontDoor>>) -> Response {
    // The switcher is read once per page load, so it always rechecks.
    let repositories: Vec<_> = front_door
        .connected(true)
        .await
        .iter()
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

/// The page's name for one checkout: derived from this service's random instance and the
/// checkout's path, so it is not a path, cannot be computed from one by anyone else and is mapped
/// back to the checkout here, never by the browser.
fn checkout_id(instance: &str, connection: &Connection) -> String {
    let mut digest = Sha256::new();
    digest.update(instance.as_bytes());
    digest.update([0]);
    digest.update(connection.root.as_os_str().as_encoded_bytes());
    format!("{:x}", digest.finalize())[..24].to_owned()
}

/// One checkout's sharing state, with the repository and Hub the page shows the person.
fn capture_row(instance: &str, connection: &Connection) -> serde_json::Value {
    let setting = crate::change_capture_client::capture_setting(&connection.root);
    json!({
        "id": checkout_id(instance, connection),
        "repository": connection.repo,
        "hub": connection.hub,
        "folder": connection.root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
        "configured": setting.is_some_and(|setting| setting.configured),
        "enabled": setting.is_some_and(|setting| setting.enabled),
        "mode": setting.and_then(|setting| setting.mode),
        "readable": setting.is_some(),
    })
}

async fn capture_settings(State(front_door): State<Arc<FrontDoor>>) -> Response {
    let instance = front_door.instance.clone();
    let rows = tokio::task::spawn_blocking(move || {
        all_connections()
            .iter()
            .map(|connection| capture_row(&instance, connection))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    Json(json!({"version": 1, "checkouts": rows})).into_response()
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptureChange {
    /// The listed checkout's id.
    id: String,
    /// The repository and Hub the person was shown for that checkout. Turning sharing on is
    /// refused if the checkout is connected to anything else by the time it is applied.
    repository: String,
    hub: String,
    mode: CaptureModeV1,
    /// Required to turn sharing on: the person has seen what leaves this computer.
    #[serde(default)]
    confirm_sharing: bool,
}

/// Turns conversation sharing on or off for one listed checkout. Sharing can start uploading
/// conversation bodies, so it needs the page's own origin, a live grant and an explicit
/// confirmation; the checkout is found by its listed id, never by a path.
async fn capture_change(
    State(front_door): State<Arc<FrontDoor>>,
    body: axum::body::Bytes,
) -> Response {
    let Ok(change) = serde_json::from_slice::<CaptureChange>(&body) else {
        return refuse(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "expected {id, repository, hub, mode, confirm_sharing}",
        );
    };
    let sharing = match change.mode {
        CaptureModeV1::SharedConversation => true,
        CaptureModeV1::MetadataOnly => false,
        CaptureModeV1::LocalConversation => {
            return refuse(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "mode must be metadata_only or shared_conversation",
            );
        }
    };
    if sharing && !change.confirm_sharing {
        return refuse(
            StatusCode::BAD_REQUEST,
            "confirmation_required",
            "turning sharing on needs confirm_sharing: true after the person has seen what is uploaded",
        );
    }
    let instance = front_door.instance.clone();
    let outcome = tokio::task::spawn_blocking(move || {
        let connection = all_connections()
            .into_iter()
            .find(|connection| checkout_id(&instance, connection) == change.id)?;
        let expected = crate::change_capture_client::ExpectedTarget {
            repository: &change.repository,
            hub: &change.hub,
        };
        let result = if sharing {
            crate::change_capture_client::enable_capture(&connection.root, change.mode, expected)
        } else {
            crate::change_capture_client::set_capture_mode(
                &connection.root,
                change.mode,
                Some(expected),
            )
        };
        Some(result.map(|_| capture_row(&instance, &connection)))
    })
    .await;
    match outcome {
        Ok(Some(Ok(row))) => Json(json!({"version": 1, "checkout": row})).into_response(),
        Ok(Some(Err(error))) => refuse(
            StatusCode::CONFLICT,
            "capture_unavailable",
            &error.to_string(),
        ),
        Ok(None) => refuse(StatusCode::NOT_FOUND, "unknown_checkout", REPOSITORY_GONE),
        Err(_) => refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            "capture_failed",
            "the change stopped unexpectedly",
        ),
    }
}

async fn disconnect(
    State(front_door): State<Arc<FrontDoor>>,
    axum::Extension(granted): axum::Extension<Granted>,
) -> Response {
    if let Some((id, _)) = front_door.grant_id(&granted.grant) {
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
    if !proxied(uri.path()) {
        return refuse(
            StatusCode::NOT_FOUND,
            "not_proxied",
            "this Hub area opens on the Hub itself",
        );
    }
    let Some(target) = hub_read_url(&selected.hub, uri.path(), uri.query()) else {
        return refuse(
            StatusCode::BAD_GATEWAY,
            "hub_unavailable",
            "the saved Hub address is not usable",
        );
    };
    let mut refused: Option<String> = None;
    for _ in 0..2 {
        let token = match front_door.hub_token(&selected, refused.as_deref()).await {
            Ok(token) => token,
            Err(error) => return hub_refusal(error, &selected.hub),
        };
        // Held only for the read itself, never while a mint for this repository is waited on.
        let Ok(_permit) = front_door.hub_reads.acquire().await else {
            return refuse(
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "this computer's Hub page is stopping",
            );
        };
        let reply = front_door
            .client
            .get(target.clone())
            .bearer_auth(&token)
            .send()
            .await;
        match reply {
            // A revoked or expired cached token: replace it once before reporting the refusal.
            Ok(reply)
                if reply.status() == reqwest::StatusCode::UNAUTHORIZED && refused.is_none() =>
            {
                refused = Some(token);
            }
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
    // A Hub refusal of this computer's token is not the browser's connection ending.
    let status = if status == StatusCode::UNAUTHORIZED {
        StatusCode::BAD_GATEWAY
    } else {
        status
    };
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
    fn hub_reads_stay_on_the_saved_hub() {
        let url = |hub, path, query| hub_read_url(hub, path, query).map(|url| url.to_string());
        assert_eq!(
            url(
                "https://hub.example.test",
                "/v1/hub/repos/a/b",
                Some("limit=5")
            )
            .as_deref(),
            Some("https://hub.example.test/v1/hub/repos/a/b?limit=5")
        );
        assert_eq!(
            url("https://hub.example.test/agentscm/", "/v1/hub/whoami", None).as_deref(),
            Some("https://hub.example.test/agentscm/v1/hub/whoami")
        );
        // Request text cannot reach another host through the path or the query.
        for (path, query) in [
            ("//other.example.test/v1/hub/whoami", None),
            ("/v1/hub/whoami", Some("@other.example.test/")),
            ("/v1/hub/whoami#@other.example.test", None),
        ] {
            let read = hub_read_url("https://hub.example.test", path, query).unwrap();
            assert_eq!(
                read.host_str(),
                Some("hub.example.test"),
                "{path} {query:?}"
            );
            assert_eq!(read.port_or_known_default(), Some(443));
        }
        assert!(hub_read_url("not a url", "/v1/hub/whoami", None).is_none());
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
    fn credentials_need_the_prefix_and_a_live_grant() {
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
        assert!(
            front_door
                .granted(&with(format!("Bearer local:{grant}:owner/name")))
                .is_some()
        );
        assert!(
            front_door
                .granted(&with(format!("Bearer {grant}")))
                .is_none()
        );
        assert!(
            front_door
                .granted(&with("Bearer local:nothex:owner/name".into()))
                .is_none()
        );
    }

    #[test]
    fn only_plain_paths_the_page_reads_are_proxied() {
        for allowed in [
            "/v1/hub/whoami",
            "/v1/hub/repos/acme/widgets/work",
            "/v1/hub/repos/acme%2Fwidgets/work",
            "/v1/hub/standup/overview",
            "/v1/hub/skill-workflows/run-1",
        ] {
            assert!(proxied(allowed), "{allowed}");
        }
        for refused in [
            "/v1/hub/tokens",
            "/v1/hub/token",
            "/v1/hub/workspace/me",
            "/v1/hub/login/github",
            "/v1/hub/session",
            "/v1/hub/repos/x/%2e%2e/%2e%2e/tokens",
            "/v1/hub/repos/x/%2E%2E/tokens",
            "/v1/hub/repos/x/../../tokens",
            "/v1/hub/repos/x/./work",
            "/v1/hub/repos//work",
            "/v1/hub/repos/x\\y",
            "/v1/hub/whoamix",
        ] {
            assert!(!proxied(refused), "{refused}");
        }
    }
}
