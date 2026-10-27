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
/// How long a Hub's reported retention is reused, and how long the page waits to learn it.
const RETENTION_TTL: Duration = Duration::from_secs(60);
const RETENTION_TIMEOUT: Duration = Duration::from_secs(3);
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
    /// Days each Hub said it keeps a shared conversation (`None`: it did not say), by Hub URL.
    retention: Mutex<HashMap<String, (Instant, Option<i64>)>>,
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
    human_session: crate::hub_cli::HubAuth,
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

impl Selected {
    pub(super) fn source_repository(&self) -> String {
        format!("{}/{}", self.origin_host, self.repository).to_ascii_lowercase()
    }
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
            retention: Mutex::new(HashMap::new()),
            client: agentscm::hub::forge_http::client()
                .map_err(|_| agentscm::Error::Invalid("HTTP client unavailable".into()))?,
        })
    }

    fn cached_retention(&self, hub: &str) -> Option<(Instant, Option<i64>)> {
        self.retention
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(hub)
            .copied()
    }

    /// Days the connection's Hub says it keeps a shared conversation, from its `whoami` with the
    /// checkout's own agent token. `None` when the Hub is unreachable, slow, older or says
    /// nothing usable; the answer (or its absence) is reused for [`RETENTION_TTL`].
    async fn retention_days(&self, connection: &Connection) -> Option<i64> {
        if let Some((at, days)) = self.cached_retention(&connection.hub)
            && at.elapsed() < RETENTION_TTL
        {
            return days;
        }
        let token_file = connection.token_file.clone();
        let token = tokio::task::spawn_blocking(move || crate::client_cli::read_token(&token_file))
            .await
            .ok()
            .and_then(Result::ok);
        let days = match token {
            Some(token) => tokio::time::timeout(
                RETENTION_TIMEOUT,
                crate::client_status::check_hub(connection, &token, None),
            )
            .await
            .ok()
            .and_then(|(check, _)| match check {
                crate::client_status::HubCheck::Valid {
                    shared_conversation_retention_days,
                    ..
                } => shared_conversation_retention_days,
                _ => None,
            }),
            None => None,
        };
        self.retention
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(connection.hub.clone(), (Instant::now(), days));
        days
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
        // Only when minting: a sign-in from before computer credentials gets one here.
        crate::client_cli::enroll(&selected.hub).await;
        session?;
        let (hub, human_session) = hub_read_token::load_exact_session(&selected.hub)?;
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
        .route("/v1/hub/devices", get(devices))
        .route("/v1/hub/devices/{id}", delete(revoke_device))
        .route("/v1/hub/{*path}", get(proxy))
        .with_state(front_door.clone())
        .merge(super::explorer::routes().with_state(state.clone()))
        .merge(super::workflow_library::routes().with_state(state.clone()))
        .merge(super::intent_review::routes().with_state(state.clone()))
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
                tokio::task::spawn_blocking(move || crate::hub_cli::is_signed_in(Some(&url)))
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
/// checkout's path. It is opaque to the browser (not a path, and mapped back to the checkout here,
/// never by the browser) and scoped to one service run, so ids change after a restart. It is a
/// name, not a secret: the identity probe exposes the instance, so anyone who also knows a path
/// could compute it. Using it still needs a live grant on the capture routes.
fn checkout_id(instance: &str, connection: &Connection) -> String {
    let mut digest = Sha256::new();
    digest.update(instance.as_bytes());
    digest.update([0]);
    digest.update(connection.root.as_os_str().as_encoded_bytes());
    format!("{:x}", digest.finalize())[..24].to_owned()
}

/// One checkout's sharing state, with the repository and Hub the page shows the person.
fn capture_row(
    instance: &str,
    connection: &Connection,
    retention_days: Option<i64>,
) -> serde_json::Value {
    let setting = crate::change_capture_client::capture_setting(connection);
    let mismatch = setting
        .as_ref()
        .and_then(|setting| setting.lineage_mismatch.as_ref());
    json!({
        "id": checkout_id(instance, connection),
        "repository": connection.repo,
        "hub": connection.hub,
        "folder": connection.root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
        "configured": setting.as_ref().is_some_and(|setting| setting.configured),
        "enabled": setting.as_ref().is_some_and(|setting| setting.enabled),
        "mode": setting.as_ref().and_then(|setting| setting.mode),
        "readable": setting.is_some(),
        "lineage_mismatch": mismatch.is_some(),
        "saved_repository": mismatch.map(|saved| saved.repository.as_str()),
        "saved_hub": mismatch.map(|saved| saved.hub.as_str()),
        // What this Hub said, or null when it did not; the page never states a number it lacks.
        "shared_conversation_retention_days": retention_days,
    })
}

async fn capture_settings(State(front_door): State<Arc<FrontDoor>>) -> Response {
    let instance = front_door.instance.clone();
    let (connections, mut rows) = tokio::task::spawn_blocking(move || {
        let connections = all_connections();
        let rows = connections
            .iter()
            .map(|connection| capture_row(&instance, connection, None))
            .collect::<Vec<_>>();
        (connections, rows)
    })
    .await
    .unwrap_or_default();
    // One lookup per Hub, all at once, so a slow Hub delays the list by one timeout at most.
    let mut lookups = tokio::task::JoinSet::new();
    let mut hubs = std::collections::BTreeSet::new();
    for connection in &connections {
        if hubs.insert(connection.hub.clone()) {
            let (door, connection) = (front_door.clone(), connection.clone());
            lookups.spawn(async move {
                (
                    connection.hub.clone(),
                    door.retention_days(&connection).await,
                )
            });
        }
    }
    let mut days: HashMap<String, Option<i64>> = HashMap::new();
    while let Some(Ok((hub, found))) = lookups.join_next().await {
        days.insert(hub, found);
    }
    for (row, connection) in rows.iter_mut().zip(&connections) {
        row["shared_conversation_retention_days"] =
            json!(days.get(&connection.hub).copied().flatten());
    }
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
        Some(result.map(|_| connection))
    })
    .await;
    match outcome {
        Ok(Some(Ok(connection))) => {
            let days = front_door
                .cached_retention(&connection.hub)
                .and_then(|(_, days)| days);
            let row = capture_row(&front_door.instance, &connection, days);
            Json(json!({"version": 1, "checkout": row})).into_response()
        }
        Ok(Some(Err(error))) => {
            // The detail can name local paths, so it stays in this process's log.
            eprintln!("local capture change refused: {error}");
            refuse(
                StatusCode::CONFLICT,
                "capture_unavailable",
                crate::change_capture_client::PageRefusal::of(&error).message(),
            )
        }
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

/// This computer's computers on the selected repository's Hub, asked with its credential,
/// with a top-level `hub` naming that Hub. One Hub per call; switching repository switches Hub.
async fn devices(
    State(front_door): State<Arc<FrontDoor>>,
    axum::Extension(selected): axum::Extension<Selected>,
) -> Response {
    let unavailable = || {
        refuse(
            StatusCode::BAD_GATEWAY,
            "hub_unavailable",
            "the Hub did not answer; try again",
        )
    };
    let reply = match device_call(
        &front_door,
        &selected,
        reqwest::Method::GET,
        "/v1/hub/devices",
    )
    .await
    {
        Ok(reply) if reply.status().is_success() => reply,
        Ok(reply) => return relay(reply).await,
        Err(response) => return response,
    };
    // A list of at most 100 computers is far below this.
    let Ok(bytes) = reply.bytes().await else {
        return unavailable();
    };
    match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(mut body) if body.is_object() && bytes.len() <= 1 << 20 => {
            body["hub"] = json!(selected.hub);
            Json(body).into_response()
        }
        _ => unavailable(),
    }
}

/// Revokes one of this person's computers on the selected repository's Hub.
async fn revoke_device(
    State(front_door): State<Arc<FrontDoor>>,
    axum::Extension(selected): axum::Extension<Selected>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Response {
    let valid = id.strip_prefix("dev_").is_some_and(|hex| {
        hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    if !valid {
        return refuse(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "that is not a computer id",
        );
    }
    // The id that goes into the Hub's URL is the one the Hub listed for this person, never the
    // request's own text: the request only chooses among them.
    let listed = match device_call(
        &front_door,
        &selected,
        reqwest::Method::GET,
        "/v1/hub/devices",
    )
    .await
    {
        Ok(reply) if reply.status().is_success() => reply.bytes().await.ok(),
        Ok(reply) => return relay(reply).await,
        Err(response) => return response,
    };
    let known = listed
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|list| {
            list["devices"].as_array()?.iter().find_map(|device| {
                let listed = device["id"].as_str()?;
                (listed == id).then(|| listed.to_owned())
            })
        });
    let Some(known) = known else {
        return refuse(
            StatusCode::NOT_FOUND,
            "not_found",
            "no such computer of yours on this Hub",
        );
    };
    let path = format!("/v1/hub/devices/{known}");
    match device_call(&front_door, &selected, reqwest::Method::DELETE, &path).await {
        Ok(reply) => relay(reply).await,
        Err(response) => response,
    }
}

async fn device_call(
    front_door: &FrontDoor,
    selected: &Selected,
    method: reqwest::Method,
    path: &str,
) -> std::result::Result<reqwest::Response, Response> {
    let hub = selected.hub.clone();
    let device = tokio::task::spawn_blocking(move || crate::hub_cli::saved_device(&hub))
        .await
        .ok()
        .and_then(Result::ok)
        .flatten();
    let Some(device) = device else {
        return Err(hub_refusal(OpenError::MissingSignIn, &selected.hub));
    };
    let Some(target) = hub_read_url(&selected.hub, path, None) else {
        return Err(refuse(
            StatusCode::BAD_GATEWAY,
            "hub_unavailable",
            "the saved Hub address is not usable",
        ));
    };
    front_door
        .client
        .request(method, target)
        .bearer_auth(&device.secret)
        .send()
        .await
        .map_err(|_| {
            refuse(
                StatusCode::BAD_GATEWAY,
                "hub_unavailable",
                "the Hub did not answer; try again",
            )
        })
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
        // The Hub's own sentence when it sent one, word for word.
        OpenError::RepositoryNotConnected(message) => refuse(
            StatusCode::FORBIDDEN,
            "not_connected_to_hub",
            message.as_deref().unwrap_or(
                "this repository is not connected to its Hub; ask the Hub's operator to connect it",
            ),
        ),
        OpenError::AccessDenied(message) => refuse(
            StatusCode::FORBIDDEN,
            "access_denied",
            message
                .as_deref()
                .unwrap_or("your Hub sign-in cannot read this repository"),
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

    async fn granted_json(
        app: &mut Router,
        credential: &str,
        method: &str,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        use axum::body::to_bytes;
        use tower_service::Service;

        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:3210")
            .header(header::ORIGIN, "http://127.0.0.1:3210")
            .header(header::AUTHORIZATION, format!("Bearer {credential}"));
        let request = if body.is_null() {
            request.body(Body::empty()).unwrap()
        } else {
            request = request.header(header::CONTENT_TYPE, "application/json");
            request.body(Body::from(body.to_string())).unwrap()
        };
        let response = app.call(request).await.unwrap();
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        let value = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, value)
    }

    fn sanitize_workflow_wire(
        value: &mut serde_json::Value,
        replacements: &HashMap<String, String>,
    ) {
        match value {
            serde_json::Value::Object(object) => {
                for (key, value) in object {
                    if matches!(key.as_str(), "created_at" | "updated_at" | "expires_at") {
                        *value = json!(1);
                    } else if key == "content" {
                        *value = json!("<captured text>");
                    } else if matches!(key.as_str(), "digest" | "sha256")
                        && value.as_str().is_some_and(|value| value.len() == 64)
                    {
                        *value = json!(
                            "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                        );
                    } else {
                        sanitize_workflow_wire(value, replacements);
                    }
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    sanitize_workflow_wire(value, replacements);
                }
            }
            serde_json::Value::String(value) => {
                for (original, replacement) in replacements {
                    if value.contains(original) {
                        *value = value.replace(original, replacement);
                    }
                }
            }
            _ => {}
        }
    }

    fn canonicalize_captured_workflow_list(value: &mut serde_json::Value) {
        let Some(workflows) = value
            .get_mut("workflows")
            .and_then(|value| value.get_mut("workflows"))
            .and_then(serde_json::Value::as_array_mut)
        else {
            return;
        };
        // Draft timestamps have one-second resolution, so tied rows are ordered by random IDs.
        workflows.sort_by(|left, right| {
            left.pointer("/draft/id")
                .and_then(serde_json::Value::as_str)
                .cmp(
                    &right
                        .pointer("/draft/id")
                        .and_then(serde_json::Value::as_str),
                )
        });
    }

    fn required_string(value: &serde_json::Value, key: &str) -> String {
        value[key].as_str().unwrap().to_owned()
    }

    #[tokio::test]
    async fn intent_reviews_require_grants_bind_selected_repository_and_fence_dispositions() {
        use agentscm::workflow_library::*;
        use tower_service::Service;

        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let front_door = Arc::new(FrontDoor::new().unwrap());
        front_door.set_origin("http://127.0.0.1:3210".into());
        let state = AppState {
            data_dir: data.clone(),
            engine: agentscm::store::Engine::open(&data).unwrap(),
            work: Arc::new(tokio::sync::Semaphore::new(2)),
            collisions: None,
            timeline: None,
            explorer: None,
            front_door: Some(front_door.clone()),
        };
        let (code, _) = front_door.mint_code().unwrap();
        let (grant, _) = front_door.redeem(&code).unwrap().unwrap();
        let credential = format!("local:{grant}:fixture/project");
        let mut app = router(state, front_door.clone());
        let uri = "/v1/local/intent-reviews";
        let (status, _) = granted_json(
            &mut app,
            "invalid",
            "POST",
            uri,
            json!({"operation":"status","work_id":"work"}),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        *front_door.connections.lock().await = Some((Instant::now(), Arc::new(vec![])));
        let (status, _) = granted_json(
            &mut app,
            &credential,
            "POST",
            uri,
            json!({"operation":"status","work_id":"work"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);

        let mut connections = Vec::new();
        for (name, forge, repository) in [
            ("checkout", "github.com", "fixture/project"),
            ("other", "github.com", "fixture/other"),
            ("other-forge", "gitlab.com", "fixture/project"),
        ] {
            let root = directory.path().join(name);
            std::fs::create_dir_all(&root).unwrap();
            let git = |args: &[&str]| {
                let output = std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(args)
                    .output()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
            };
            git(&["init", "-q"]);
            git(&["config", "user.name", "Fixture"]);
            git(&["config", "user.email", "fixture@example.invalid"]);
            git(&["config", "commit.gpgsign", "false"]);
            git(&[
                "remote",
                "add",
                "origin",
                &format!("https://{forge}/{repository}.git"),
            ]);
            std::fs::write(root.join("limit.txt"), "one limit per account\n").unwrap();
            git(&["add", "limit.txt"]);
            git(&["commit", "-qm", "Synthetic initial source"]);
            connections.push(Connection {
                root,
                origin_host: forge.into(),
                hub: "https://hub.example.test".into(),
                repo: repository.into(),
                token_file: directory.path().join("unused-token"),
                report_completion: false,
                owned_token_id: None,
                owned_token_expires_at: None,
                agents: Vec::new(),
            });
        }
        let checkout = connections[0].root.clone();
        let other_forge = connections.pop().unwrap();
        let connections = Arc::new(connections);
        *front_door.connections.lock().await = Some((Instant::now(), connections.clone()));
        let (status, _) = granted_json(
            &mut app,
            &credential,
            "GET",
            "/v1/local/intent-reviews/work",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(!data.join("skill-workflows").exists());
        let mut library = Library::open(&data).unwrap();
        let preview = library
            .save_preview(
                PreviewSkill {
                    name: "Intent review".into(),
                    entrypoint: "SKILL.md".into(),
                    provenance: Provenance::Markdown {
                        locator: "pasted_markdown".into(),
                    },
                },
                &[PackageFile {
                    path: "SKILL.md".into(),
                    content: "# Intent review\nCompare human criteria with evidence.".into(),
                }],
                vec![],
            )
            .unwrap();
        let skill = library
            .accept_import(&AcceptImport {
                preview_id: preview.preview_id,
                digest: preview.digest,
                idempotency_key: "import".into(),
            })
            .unwrap();
        let draft = library
            .create_workflow(&CreateWorkflow {
                name: "Intent review".into(),
                summary: "Synthetic review".into(),
                idempotency_key: "create".into(),
            })
            .unwrap();
        let draft = library
            .update_workflow(
                &draft.id,
                &UpdateWorkflow {
                    expected_generation: draft.generation,
                    name: None,
                    summary: None,
                    steps: Some(vec![WorkflowStep {
                        id: "review".into(),
                        label: "Review".into(),
                        needs: vec![],
                        required: true,
                        kind: WorkflowStepKind::Skill {
                            skill_version_id: skill.id,
                        },
                    }]),
                    trigger: None,
                    follow_on: None,
                },
            )
            .unwrap();
        let Publication::Published { version, .. } = library
            .publish(
                &draft.id,
                &PublishWorkflow {
                    expected_generation: draft.generation,
                    idempotency_key: "publish".into(),
                },
            )
            .unwrap()
        else {
            panic!("fixture workflow did not publish")
        };
        let start = json!({"operation":"start","request_id":"start","work_id":"work","workflow_version_id":version.id,"step_id":"review","contract":{"version":1,"previous_digest":null,"request":[{"id":"human-1","kind":"human_message","source":"visible-message-1","author":"caller-supplied-human","recorded_at_ms":1,"sha256":"derived-by-host","coverage":"complete","text":"Use one limit per account."}],"amendments":[],"criteria":[{"id":"account","description":"Use one limit per account.","sources":["human-1"],"required":true,"constraint":false}],"focus":[],"coverage":"complete","gaps":[]},"scope":{"parent_work_id":null,"checkpoint":null,"criteria":["account"],"coverage":"complete","gaps":[]},"limits":{"max_correction_rounds":1,"max_invocations":4,"wall_time_ms":600000},"evidence":[{"id":"check","text":"Synthetic caller-reported account check.","origin":"caller report","coverage":"complete"}]});
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let (status, created) =
            granted_json(&mut app, &credential, "POST", uri, start.clone()).await;
        assert_eq!(status, StatusCode::OK, "{created}");
        assert_eq!(
            created["view"]["record"]["subject"]["repository"],
            "github.com/fixture/project"
        );
        let work_uri = "/v1/local/intent-reviews/work";
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let (status, current) = granted_json(
            &mut app,
            &credential,
            "GET",
            work_uri,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{current}");
        assert_eq!(current["next"], "assess");
        let other_credential = format!("local:{grant}:fixture/other");
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let (status, _) = granted_json(
            &mut app,
            &other_credential,
            "GET",
            work_uri,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        *front_door.connections.lock().await = Some((Instant::now(), Arc::new(vec![other_forge])));
        let (status, _) = granted_json(
            &mut app,
            &credential,
            "GET",
            work_uri,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let (status, _) = granted_json(&mut app, &credential, "POST", uri,
            json!({"operation":"claim","request_id":"foreign-claim","work_id":"work","attempt_id":"foreign-attempt","session_id":"session","phase":"assessment","lease_ms":60000})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let (status, separate) = granted_json(&mut app, &credential, "POST", uri, start).await;
        assert_eq!(status, StatusCode::OK, "{separate}");
        assert_eq!(
            separate["view"]["record"]["subject"]["repository"],
            "gitlab.com/fixture/project"
        );
        *front_door.connections.lock().await = Some((Instant::now(), connections));
        let (status, original) = granted_json(
            &mut app,
            &credential,
            "GET",
            work_uri,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{original}");
        assert_eq!(original["root_invocations"], 0);
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let (status, claimed) = granted_json(&mut app, &credential, "POST", uri, json!({"operation":"claim","request_id":"claim","work_id":"work","attempt_id":"attempt","session_id":"session","phase":"assessment","lease_ms":60000})).await;
        assert_eq!(status, StatusCode::OK, "{claimed}");
        assert_eq!(claimed["task"]["skill"]["files"][0]["path"], "SKILL.md");
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let (status, reported) = granted_json(&mut app, &credential, "POST", uri, json!({"operation":"report","request_id":"report","work_id":"work","attempt_id":"attempt","session_id":"session","fence":1,"execution":"succeeded","assessment":{"criteria":[{"criterion":"account","status":"supported","explanation":"Synthetic caller evidence supports this criterion.","evidence_ids":["check"],"corrective_outcome":null,"recheck":null}],"coverage":"complete","gaps":[]}})).await;
        assert_eq!(status, StatusCode::OK, "{reported}");
        assert_eq!(reported["view"]["next"], "complete");
        let expected_subject_digest = reported["view"]["current_subject_digest"].clone();
        std::fs::write(checkout.join("limit.txt"), "changed candidate\n").unwrap();
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let (status, refused) = granted_json(&mut app, &credential, "POST", uri, json!({"operation":"disposition","request_id":"disposition","work_id":"work","expected_subject_digest":expected_subject_digest,"kind":"exception","criteria":["account"],"reason":"Synthetic explicit exception","authorization":{"id":"human-2","kind":"human_message","source":"visible-message-2","author":"caller-supplied-human","recorded_at_ms":2,"sha256":"derived-by-host","coverage":"complete","text":"Accept this explicit exception."}})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{refused}");
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let (status, current) = granted_json(
            &mut app,
            &credential,
            "GET",
            work_uri,
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(current["current"], false);
        assert_eq!(current["record"]["dispositions"], json!([]));
        front_door.connections.lock().await.as_mut().unwrap().0 = Instant::now();
        let oversized = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::HOST, "127.0.0.1:3210")
            .header(header::ORIGIN, "http://127.0.0.1:3210")
            .header(header::AUTHORIZATION, format!("Bearer {credential}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(" ".repeat(512 * 1024 + 1)))
            .unwrap();
        assert_eq!(
            app.call(oversized).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[tokio::test]
    async fn workflow_library_writes_need_a_browser_grant_and_selected_checkout() {
        use tower_service::Service;

        let directory = tempfile::tempdir().unwrap();
        let front_door = Arc::new(FrontDoor::new().unwrap());
        front_door.set_origin("http://127.0.0.1:3210".into());
        let state = AppState {
            data_dir: directory.path().to_path_buf(),
            engine: agentscm::store::Engine::open(directory.path()).unwrap(),
            work: Arc::new(tokio::sync::Semaphore::new(2)),
            collisions: None,
            timeline: None,
            explorer: None,
            front_door: Some(front_door.clone()),
        };
        let request = |authorization: Option<String>| {
            let mut request = Request::builder()
                .method("POST")
                .uri("/v1/local/workflow-library")
                .header(header::HOST, "127.0.0.1:3210")
                .header(header::ORIGIN, "http://127.0.0.1:3210")
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(
                    r#"{"name":"release","summary":"release","idempotency_key":"one"}"#,
                ))
                .unwrap();
            if let Some(authorization) = authorization {
                request.headers_mut().insert(
                    header::AUTHORIZATION,
                    HeaderValue::from_str(&authorization).unwrap(),
                );
            }
            request
        };
        let mut app = router(state, front_door.clone());
        let response = app
            .call(request(Some(format!("Bearer {}", "a".repeat(64)))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(!directory.path().join("workflow-library").exists());

        let (code, _) = front_door.mint_code().unwrap();
        let (grant, _) = front_door.redeem(&code).unwrap().unwrap();
        let response = app
            .call(request(Some(format!("Bearer local:{grant}:owner/name"))))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert!(!directory.path().join("workflow-library").exists());
    }

    #[tokio::test]
    async fn workflow_library_granted_router_round_trip_matches_the_ui_wire_fixture() {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let checkout = directory.path().join("checkout");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        let front_door = Arc::new(FrontDoor::new().unwrap());
        front_door.set_origin("http://127.0.0.1:3210".into());
        *front_door.connections.lock().await = Some((
            Instant::now(),
            Arc::new(vec![Connection {
                root: checkout,
                origin_host: "github.com".into(),
                hub: "https://hub.example.test".into(),
                repo: "acme/widgets".into(),
                token_file: directory.path().join("unused-token"),
                report_completion: false,
                owned_token_id: None,
                owned_token_expires_at: None,
                agents: Vec::new(),
            }]),
        ));
        let state = AppState {
            data_dir: data.clone(),
            engine: agentscm::store::Engine::open(&data).unwrap(),
            work: Arc::new(tokio::sync::Semaphore::new(2)),
            collisions: None,
            timeline: None,
            explorer: None,
            front_door: Some(front_door.clone()),
        };
        let (code, _) = front_door.mint_code().unwrap();
        let (grant, _) = front_door.redeem(&code).unwrap().unwrap();
        let credential = format!("local:{grant}:acme/widgets");
        let mut app = router(state, front_door);
        let mut captured = serde_json::Map::new();
        let mut replacements = HashMap::new();

        let (status, preview) = granted_json(
            &mut app,
            &credential,
            "POST",
            "/v1/local/skill-imports/preview",
            json!({"source":{"kind":"markdown","name":"Release helper","markdown":"# Release helper\n","resources":[
                {"path":"draft.json","content":"{\"version\":1,\"inputs\":{},\"output\":{\"name\":\"draft\",\"media_type\":\"text/markdown\",\"max_bytes\":16384},\"action\":{\"kind\":\"prompt\",\"prompt\":\"draft.md\",\"required_tools\":[]}}"},
                {"path":"draft.md","content":"Write release notes."}
            ]}}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        replacements.insert(
            required_string(&preview, "preview_id"),
            "imp_fixture".into(),
        );
        captured.insert("preview".into(), preview.clone());

        let (status, accepted) = granted_json(
            &mut app,
            &credential,
            "POST",
            "/v1/local/skill-imports/accept",
            json!({"preview_id":preview["preview_id"],"digest":preview["digest"],"idempotency_key":"accept-release-helper"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let skill_id = required_string(&accepted, "id");
        replacements.insert(skill_id.clone(), "skl_fixture".into());
        captured.insert("accepted_skill".into(), accepted.clone());

        let (status, skills) = granted_json(
            &mut app,
            &credential,
            "GET",
            "/v1/local/skill-library",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        captured.insert("skills".into(), skills);

        let (status, child) = granted_json(
            &mut app,
            &credential,
            "POST",
            "/v1/local/workflow-library",
            json!({"name":"Release action","summary":"Create release notes.","idempotency_key":"create-release-action"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let child_id = required_string(&child, "id");
        replacements.insert(child_id.clone(), "wfl_child".into());
        captured.insert("child_created".into(), child.clone());
        let action_step = json!({"id":"draft-release","label":"Draft release notes","needs":[],"required":true,"kind":"action","skill_version_id":skill_id,"resource":"draft.json"});
        let (status, child_updated) = granted_json(
            &mut app,
            &credential,
            "PATCH",
            &format!("/v1/local/workflow-library/{child_id}"),
            json!({"expected_generation":child["generation"],"steps":[action_step],"trigger":null,"follow_on":null}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        captured.insert("child_updated".into(), child_updated.clone());
        let (status, child_publish) = granted_json(
            &mut app,
            &credential,
            "POST",
            &format!("/v1/local/workflow-library/{child_id}/versions"),
            json!({"expected_generation":child_updated["generation"],"idempotency_key":"publish-release-action"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "child publish: {child_publish}");
        let child_version_id = required_string(&child_publish["version"], "id");
        replacements.insert(child_version_id.clone(), "wfv_child".into());
        captured.insert("child_published".into(), child_publish);

        let (status, parent) = granted_json(
            &mut app,
            &credential,
            "POST",
            "/v1/local/workflow-library",
            json!({"name":"Major feature follow-up","summary":"Prepare release evidence after merge.","idempotency_key":"create-major-follow-up"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let parent_id = required_string(&parent, "id");
        replacements.insert(parent_id.clone(), "wfl_parent".into());
        captured.insert("parent_created".into(), parent.clone());
        let parent_steps = json!([
            {"id":"prepare","label":"Prepare notes","needs":[],"required":true,"kind":"action","skill_version_id":skill_id,"resource":"draft.json"},
            {"id":"reuse","label":"Reuse release action","needs":["prepare"],"required":true,"kind":"workflow","workflow_version_id":child_version_id,"inputs":{}}
        ]);
        let trigger =
            json!({"kind":"confirmed_merge","target":"main","labels_all":["major-feature"]});
        let follow_on =
            json!({"workflow_version_id":child_version_id,"when":"success","inputs":{}});
        let (status, parent_updated) = granted_json(
            &mut app,
            &credential,
            "PATCH",
            &format!("/v1/local/workflow-library/{parent_id}"),
            json!({"expected_generation":parent["generation"],"steps":parent_steps,"trigger":trigger,"follow_on":follow_on}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        captured.insert("parent_updated".into(), parent_updated.clone());
        let (status, parent_cleared) = granted_json(
            &mut app,
            &credential,
            "PATCH",
            &format!("/v1/local/workflow-library/{parent_id}"),
            json!({"expected_generation":parent_updated["generation"],"trigger":null,"follow_on":null}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(parent_cleared["trigger"].is_null());
        assert!(parent_cleared["follow_on"].is_null());
        captured.insert("parent_cleared".into(), parent_cleared.clone());
        let (status, parent_restored) = granted_json(
            &mut app,
            &credential,
            "PATCH",
            &format!("/v1/local/workflow-library/{parent_id}"),
            json!({"expected_generation":parent_cleared["generation"],"trigger":trigger,"follow_on":follow_on}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        captured.insert("parent_restored".into(), parent_restored.clone());
        let (status, parent_publish) = granted_json(
            &mut app,
            &credential,
            "POST",
            &format!("/v1/local/workflow-library/{parent_id}/versions"),
            json!({"expected_generation":parent_restored["generation"],"idempotency_key":"publish-major-follow-up"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let parent_version_id = required_string(&parent_publish["version"], "id");
        replacements.insert(parent_version_id.clone(), "wfv_parent".into());
        captured.insert("parent_published".into(), parent_publish);

        let (status, workflows) = granted_json(
            &mut app,
            &credential,
            "GET",
            "/v1/local/workflow-library",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        captured.insert("workflows".into(), workflows);
        let (status, simulation) = granted_json(
            &mut app,
            &credential,
            "POST",
            "/v1/local/workflow-simulations",
            json!({"workflow_version_id":parent_version_id,"repository":"acme/widgets","source_revision":"1111111111111111111111111111111111111111"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        captured.insert("simulation".into(), simulation);

        let attachment_path =
            "/v1/local/repositories/acme%2Fwidgets/workflow-attachments/major-feature";
        let (status, attachment) = granted_json(
            &mut app,
            &credential,
            "PUT",
            attachment_path,
            json!({"workflow_version_id":parent_version_id,"params":{},"idempotency_key":"attach-major-feature"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        captured.insert("attachment_put".into(), attachment.clone());
        let (status, attachments) = granted_json(
            &mut app,
            &credential,
            "GET",
            "/v1/local/repositories/acme%2Fwidgets/workflow-attachments",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        captured.insert("attachments_reloaded".into(), attachments);
        let (status, validation) = granted_json(
            &mut app,
            &credential,
            "POST",
            &format!("{attachment_path}/validate"),
            json!({"expected_generation":attachment["generation"],"source_revision":"1111111111111111111111111111111111111111"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        captured.insert("attachment_validated".into(), validation);

        let (status, invalid) = granted_json(
            &mut app,
            &credential,
            "POST",
            "/v1/local/workflow-library",
            json!({"name":"Invalid publication","summary":"Exercise structured lint diagnostics.","idempotency_key":"create-invalid-publication"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let invalid_id = required_string(&invalid, "id");
        replacements.insert(invalid_id.clone(), "wfl_invalid".into());
        let invalid_step = json!({"id":"missing-action","label":"Missing action","needs":[],"required":true,"kind":"action","skill_version_id":skill_id,"resource":"missing.json"});
        let (status, invalid_updated) = granted_json(
            &mut app,
            &credential,
            "PATCH",
            &format!("/v1/local/workflow-library/{invalid_id}"),
            json!({"expected_generation":invalid["generation"],"steps":[invalid_step],"trigger":null,"follow_on":null}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, publication_rejected) = granted_json(
            &mut app,
            &credential,
            "POST",
            &format!("/v1/local/workflow-library/{invalid_id}/versions"),
            json!({"expected_generation":invalid_updated["generation"],"idempotency_key":"publish-invalid-publication"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(publication_rejected["error"], "invalid_workflow");
        assert!(
            publication_rejected["diagnostics"]
                .as_array()
                .is_some_and(|items| !items.is_empty())
        );
        captured.insert("publication_rejected".into(), publication_rejected);

        let (status, conflict) = granted_json(
            &mut app,
            &credential,
            "PATCH",
            &format!("/v1/local/workflow-library/{parent_id}"),
            json!({"expected_generation":1,"name":"stale edit"}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(conflict["error"].as_str().is_some());
        captured.insert("conflict".into(), conflict);

        let mut captured = serde_json::Value::Object(captured);
        sanitize_workflow_wire(&mut captured, &replacements);
        canonicalize_captured_workflow_list(&mut captured);
        let mut expected: serde_json::Value = serde_json::from_str(include_str!(
            "../../ui/tests/fixtures/workflow-library-router.json"
        ))
        .unwrap();
        canonicalize_captured_workflow_list(&mut expected);
        assert_eq!(captured, expected);
    }

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
