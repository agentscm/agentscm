//! Human sign-in through GitHub or GitLab, in-memory sessions, and the routes
//! that let a signed-in person mint, list and revoke agent tokens.
use std::collections::HashMap;
use std::fmt;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Mutex, MutexGuard, PoisonError};

use axum::Extension;
use axum::Router;
use axum::extract::{ConnectInfo, FromRequestParts, Path, RawQuery, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::forge_http::{Forge, check_base_url};
use super::http::{ApiError, JsonBody, Reader, bearer, blocking};
use super::store::{
    ForgeIdentity, MAX_AGENT_TOKEN_TTL_SECONDS, User, mint_bound_agent_token, unix_now,
    validate_agent_name, validate_login, validate_repository,
};
use super::{HubError, HubState, Result};
use crate::team::auth::{generate_secret, token_digest};

mod devices;

const SESSION_DIGEST_DOMAIN: &[u8] = b"agentscm.hub.session.v1\0";
const COOKIE: &str = "__Host-agentscm_login";
/// The web pages' sign-in. The secret is only in this HttpOnly cookie; the Hub keeps its digest.
const BROWSER_COOKIE: &str = "__Host-agentscm_session";
/// Every request that changes something with the web sign-in names this token in this header.
const CSRF_HEADER: &str = "x-csrf-token";
const CSRF_DOMAIN: &[u8] = b"agentscm.hub.csrf.v1\0";
const SESSION_SECONDS: i64 = 8 * 60 * 60;
const BROWSER_LOGIN_SECONDS: i64 = 600;
const HANDOFF_SECONDS: i64 = 60;
const MAX_DEVICE_SECONDS: i64 = 900;
const MAX_ENTRIES: usize = 1024;
/// Unauthenticated starts one client may have pending, so one client cannot fill the table.
const MAX_PENDING_PER_CLIENT: usize = 8;
const MAX_SESSIONS_PER_USER: usize = 8;
const MAX_SECRET_FILE_BYTES: u64 = 4096;
/// Connected repositories a sign-in tries before it gives up.
const MAX_SIGN_IN_PROBES: usize = 200;
const GITLAB_REPORTER: i64 = 20;
const GITLAB_DEVELOPER: i64 = 30;

// shortcut: sessions and pending logins live in hub memory, so a restart or a second hub replica signs everyone out; move them to the store when the hub runs replicated

struct Provider {
    name: &'static str,
    client_id: String,
    client_secret: String,
    web: String,
    api: String,
    /// Identity host: the web URL's host, with the port when not the default.
    host: String,
}

impl fmt::Debug for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Provider")
            .field("name", &self.name)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("web", &self.web)
            .field("api", &self.api)
            .field("host", &self.host)
            .finish()
    }
}

impl Provider {
    fn is_gitlab(&self) -> bool {
        self.name == "gitlab"
    }

    fn token_url(&self) -> String {
        let path = if self.is_gitlab() {
            "/oauth/token"
        } else {
            "/login/oauth/access_token"
        };
        format!("{}{path}", self.web)
    }
}

#[derive(Debug)]
pub struct LoginConfig {
    public_url: String,
    providers: Vec<Provider>,
    admins: Vec<Entry>,
    /// Who may sign in besides the admins; empty means admins only.
    allowed: Vec<Entry>,
}

#[derive(Debug)]
enum Who {
    /// Lowercase.
    Login(String),
    Id(i64),
}

/// `provider:login` or `provider:#id`.
#[derive(Debug)]
struct Entry {
    provider: String,
    who: Who,
}

fn entry(var: &str, text: &str) -> Result<Entry> {
    let bad = || {
        HubError::Invalid(format!(
            "{var} must be provider:login or provider:#id, comma separated, for github or gitlab"
        ))
    };
    let (provider, who) = text.trim().split_once(':').ok_or_else(bad)?;
    if !matches!(provider, "github" | "gitlab") {
        return Err(bad());
    }
    let who = match who.strip_prefix('#') {
        Some(id) if id.bytes().all(|b| b.is_ascii_digit()) => {
            Who::Id(id.parse().ok().filter(|id| *id > 0).ok_or_else(bad)?)
        }
        Some(_) => return Err(bad()),
        None if validate_login(who).is_ok() && !who.contains(char::is_whitespace) => {
            Who::Login(who.to_ascii_lowercase())
        }
        None => return Err(bad()),
    };
    Ok(Entry {
        provider: provider.to_owned(),
        who,
    })
}

fn listed(list: &[Entry], provider: &str, id: i64, login: &str) -> bool {
    list.iter().any(|entry| {
        entry.provider == provider
            && match &entry.who {
                Who::Id(listed) => *listed == id,
                Who::Login(listed) => listed.eq_ignore_ascii_case(login),
            }
    })
}

impl LoginConfig {
    /// Reads `AGENTSCM_{GITHUB,GITLAB}_*`, `AGENTSCM_ADMINS` and
    /// `AGENTSCM_ALLOWED_USERS` through `lookup`. A `login` entry matches the
    /// login the forge returns at sign-in, on the provider's configured host,
    /// so a renamed or re-registered account changes who matches; `#id` entries
    /// match the forge's numeric user id and are the safe form.
    pub fn from_lookup(public_url: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let lookup = |name: &str| lookup(name).filter(|value| !value.trim().is_empty());
        let providers: Vec<_> = [
            provider("github", "GITHUB", "https://github.com", &lookup)?,
            provider("gitlab", "GITLAB", "https://gitlab.com", &lookup)?,
        ]
        .into_iter()
        .flatten()
        .collect();
        // Only a hub with login needs a usable public URL.
        let public_url = if providers.is_empty() {
            String::new()
        } else {
            check_base_url(public_url).map_err(|_| {
                HubError::Invalid(
                    "the hub's public URL is not usable (https://host, or http:// on a \
                     loopback IP); set --public-url"
                        .into(),
                )
            })?
        };
        let list = |var: &str| -> Result<Vec<Entry>> {
            lookup(var).map_or(Ok(Vec::new()), |list| {
                list.split(',').map(|text| entry(var, text)).collect()
            })
        };
        Ok(Self {
            public_url,
            providers,
            admins: list("AGENTSCM_ADMINS")?,
            allowed: list("AGENTSCM_ALLOWED_USERS")?,
        })
    }
}

fn provider(
    name: &'static str,
    prefix: &str,
    default_web: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<Option<Provider>> {
    let var = |suffix: &str| format!("AGENTSCM_{prefix}_{suffix}");
    let Some(client_id) = lookup(&var("CLIENT_ID")) else {
        return Ok(None);
    };
    let client_secret = optional_secret(lookup, &var("CLIENT_SECRET_FILE"), &var("CLIENT_SECRET"))?
        .ok_or_else(|| {
            HubError::Invalid(format!(
                "{} or {} is required with {}",
                var("CLIENT_SECRET_FILE"),
                var("CLIENT_SECRET"),
                var("CLIENT_ID")
            ))
        })?;
    let ForgeUrls { web, api, host } = forge_urls(name, prefix, default_web, lookup)?;
    Ok(Some(Provider {
        name,
        client_id,
        client_secret,
        web,
        api,
        host,
    }))
}

/// A forge's web and API URLs and the identity host, from `AGENTSCM_<prefix>_URL`
/// and `_API_URL` or their defaults. Shared by sign-in and GitHub sync.
pub(super) struct ForgeUrls {
    pub web: String,
    pub api: String,
    pub host: String,
}

pub(super) fn forge_urls(
    name: &'static str,
    prefix: &str,
    default_web: &str,
    lookup: &impl Fn(&str) -> Option<String>,
) -> Result<ForgeUrls> {
    let var = |suffix: &str| format!("AGENTSCM_{prefix}_{suffix}");
    let web = check_base_url(&lookup(&var("URL")).unwrap_or_else(|| default_web.to_owned()))?;
    let api = match lookup(&var("API_URL")) {
        Some(api) => check_base_url(&api)?,
        None if name == "gitlab" => format!("{web}/api/v4"),
        None if web == "https://github.com" => "https://api.github.com".to_owned(),
        None => format!("{web}/api/v3"),
    };
    let url = Url::parse(&web).map_err(|_| HubError::Invalid("invalid forge URL".into()))?;
    let host = match (url.host_str(), url.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_owned(),
        _ => return Err(HubError::Invalid("invalid forge URL".into())),
    };
    ForgeIdentity {
        provider: name.into(),
        host: host.clone(),
        user_id: 1,
    }
    .validate()?;
    Ok(ForgeUrls { web, api, host })
}

/// A secret from `file_var` (a private regular file) or `var`; both set is an error.
pub(super) fn optional_secret(
    lookup: &impl Fn(&str) -> Option<String>,
    file_var: &str,
    var: &str,
) -> Result<Option<String>> {
    match (lookup(file_var), lookup(var)) {
        (Some(_), Some(_)) => Err(HubError::Invalid(format!(
            "set only one of {file_var} and {var}"
        ))),
        (Some(path), None) => read_secret_file(&path, file_var).map(Some),
        (None, secret) => Ok(secret),
    }
}

/// A regular file, not readable by group or others, holding the secret.
pub(super) fn read_secret_file(path: &str, var: &str) -> Result<String> {
    let bad = |why: &str| HubError::Invalid(format!("{var} {why}"));
    // Check before opening so a FIFO cannot block startup.
    let meta = std::fs::metadata(path).map_err(|_| bad("cannot be read"))?;
    if !meta.is_file() {
        return Err(bad("must be a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return Err(bad("must not be accessible by group or others"));
        }
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .and_then(|file| {
            file.take(MAX_SECRET_FILE_BYTES + 1)
                .read_to_string(&mut text)
        })
        .map_err(|_| bad("cannot be read"))?;
    if text.len() as u64 > MAX_SECRET_FILE_BYTES {
        return Err(bad("is too large"));
    }
    let secret = text.trim_end_matches(['\r', '\n']);
    if secret.is_empty() {
        return Err(bad("is empty"));
    }
    Ok(secret.to_owned())
}

/// An in-memory table of entries that expire. When full, expired entries go
/// first, then the earliest-expiring one, so a flood cannot lock real users out.
pub(super) struct Table<T>(HashMap<String, (i64, T)>);

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self(HashMap::new())
    }
}

impl<T> Table<T> {
    pub(super) fn insert(&mut self, key: String, expires_at: i64, value: T, now: i64) {
        if self.0.len() >= MAX_ENTRIES {
            self.0.retain(|_, (expires_at, _)| *expires_at > now);
        }
        if self.0.len() >= MAX_ENTRIES {
            let oldest = self.0.iter().min_by_key(|(_, (at, _))| *at);
            if let Some(oldest) = oldest.map(|(key, _)| key.clone()) {
                self.0.remove(&oldest);
            }
        }
        self.0.insert(key, (expires_at, value));
    }

    pub(super) fn count(&self, now: i64, mine: impl Fn(&T) -> bool) -> usize {
        self.0
            .values()
            .filter(|(expires_at, value)| *expires_at > now && mine(value))
            .count()
    }

    pub(super) fn get(&self, key: &str, now: i64) -> Option<&T> {
        self.0
            .get(key)
            .filter(|(expires_at, _)| *expires_at > now)
            .map(|(_, value)| value)
    }

    /// Drops every entry whose key starts with `prefix`.
    pub(super) fn remove_prefixed(&mut self, prefix: &str) {
        self.0.retain(|key, _| !key.starts_with(prefix));
    }

    pub(super) fn take(&mut self, key: &str, now: i64) -> Option<T> {
        self.0
            .remove(key)
            .filter(|(expires_at, _)| *expires_at > now)
            .map(|(_, value)| value)
    }
}

impl Table<Human> {
    /// At most `MAX_SESSIONS_PER_USER` live sessions per person: the earliest-expiring goes.
    fn insert_session(&mut self, key: String, expires_at: i64, human: Human, now: i64) {
        let mut mine: Vec<_> = self
            .0
            .iter()
            .filter(|(_, (at, other))| *at > now && other.user.id == human.user.id)
            .map(|(key, (at, _))| (*at, key.clone()))
            .collect();
        mine.sort();
        let excess = (mine.len() + 1).saturating_sub(MAX_SESSIONS_PER_USER);
        for (_, key) in mine.into_iter().take(excess) {
            self.0.remove(&key);
        }
        self.insert(key, expires_at, human, now);
    }
}

struct BrowserLogin {
    provider: String,
    verifier: String,
    cli_port: u16,
    cli_challenge: String,
    workspace: bool,
    /// Signs the browser itself in to the Hub's web pages.
    web: bool,
    client: IpAddr,
}

struct DeviceLogin {
    client: IpAddr,
    provider: String,
    device_code: String,
    interval: i64,
}

struct Handoff {
    challenge: String,
    signed: Signed,
}

/// A signed-in person. The forge token is only used server-side.
#[derive(Clone)]
pub(super) struct Human {
    /// Digest of the session secret: its key in the table.
    key: String,
    pub(super) user: User,
    admin: bool,
    forge_token: String,
    expires_at: i64,
    /// The computer signed in by the same sign-in; revoking it ends this session too.
    device: Option<String>,
    /// Set for a web-page sign-in, which lives in the database instead of this process.
    browser: Option<BrowserSession>,
}

#[derive(Clone)]
pub(super) struct BrowserSession {
    digest: super::store::TokenDigest,
    csrf: String,
}

/// The value of the only web sign-in cookie, if there is exactly one.
fn browser_cookie(headers: &HeaderMap) -> Option<&str> {
    let mut found = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|cookie| {
            cookie
                .trim()
                .strip_prefix(BROWSER_COOKIE)?
                .strip_prefix('=')
        });
    match (found.next(), found.next()) {
        (Some(value), None) => Some(value),
        _ => None,
    }
}

fn csrf_token(secret: &str) -> Result<String> {
    token_digest(CSRF_DOMAIN, secret).map_err(|_| HubError::Unauthorized)
}

fn browser_cookie_attributes(value: &str, max_age: i64) -> String {
    format!(
        "{BROWSER_COOKIE}={value}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={max_age}"
    )
}

#[derive(Clone)]
struct Signed {
    secret: String,
    expires_at: i64,
    user: User,
    admin: bool,
}

#[derive(Default)]
struct Pending {
    browser: Table<BrowserLogin>,
    device: Table<DeviceLogin>,
    handoffs: Table<Handoff>,
    sessions: Table<Human>,
    /// Recent role checks of the All repositories list, keyed by person, connection and
    /// repository: true when readable. Only forge answers are kept, never a failure.
    role_checks: Table<bool>,
    /// Providers whose forge app refused device sign-in when last asked.
    no_device: std::collections::BTreeSet<String>,
}

pub struct Login {
    config: LoginConfig,
    /// Some when any provider is enabled.
    forge: Option<Forge>,
    pending: Mutex<Pending>,
}

impl Login {
    pub fn new(config: LoginConfig) -> Result<Self> {
        let forge = if config.providers.is_empty() {
            None
        } else {
            Some(Forge::new()?)
        };
        Ok(Self {
            config,
            forge,
            pending: Mutex::new(Pending::default()),
        })
    }

    pub fn unconfigured() -> Self {
        Self {
            config: LoginConfig {
                public_url: String::new(),
                providers: Vec::new(),
                admins: Vec::new(),
                allowed: Vec::new(),
            },
            forge: None,
            pending: Mutex::new(Pending::default()),
        }
    }

    /// The configured sign-in providers; empty when none is.
    pub fn enabled(&self) -> Vec<&'static str> {
        self.config.providers.iter().map(|p| p.name).collect()
    }

    /// Never held across an await.
    fn pending(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn provider(&self, name: &str) -> Option<(&Provider, &Forge)> {
        let provider = self.config.providers.iter().find(|p| p.name == name)?;
        Some((provider, self.forge.as_ref()?))
    }

    fn redirect_uri(&self, provider: &str) -> String {
        format!(
            "{}/v1/hub/login/{provider}/callback",
            self.config.public_url
        )
    }

    fn is_admin(&self, provider: &str, id: i64, login: &str) -> bool {
        listed(&self.config.admins, provider, id, login)
    }

    /// True when the configured admin list names this stored user.
    pub(super) fn is_admin_user(&self, user: &super::store::User) -> bool {
        self.is_admin(&user.identity.provider, user.identity.user_id, &user.login)
    }

    fn has_allow_list(&self) -> bool {
        !self.config.allowed.is_empty()
    }

    fn is_allowed(&self, provider: &str, id: i64, login: &str) -> bool {
        listed(&self.config.allowed, provider, id, login)
    }

    fn session(&self, digest: &str) -> Option<Human> {
        self.pending().sessions.get(digest, unix_now()).cloned()
    }

    /// Ends the sessions of the sign-in that signed in `device`, so a revoked computer's saved
    /// session cannot enroll it again.
    fn drop_sessions_of_device(&self, device: &str) {
        self.pending()
            .sessions
            .0
            .retain(|_, (_, human)| human.device.as_deref() != Some(device));
    }

    fn drop_sessions_of(&self, user: &str) {
        self.pending()
            .sessions
            .0
            .retain(|_, (_, human)| human.user.id != user);
    }
}

pub(super) fn random_hex() -> Result<String> {
    generate_secret()
        .map(|secret| secret.expose().to_owned())
        .map_err(|_| HubError::Unavailable("random source".into()))
}

fn session_digest(raw: &str) -> Result<String> {
    token_digest(SESSION_DIGEST_DOMAIN, raw).map_err(|_| HubError::Unauthorized)
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn failed() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "login_failed")
}

fn forbidden() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "forbidden")
}

fn not_found() -> ApiError {
    ApiError::new(StatusCode::NOT_FOUND, "not_found")
}

/// A valid human session, from `Authorization: Bearer <session secret>`. Agent
/// tokens never match here, and sessions never authenticate as agents.
pub struct HumanSession(Human);

impl FromRequestParts<HubState> for HumanSession {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &HubState,
    ) -> std::result::Result<Self, ApiError> {
        if parts.headers.contains_key(header::AUTHORIZATION) {
            let digest = session_digest(bearer(parts)?)?;
            return state
                .login
                .session(&digest)
                .map(Self)
                .ok_or_else(ApiError::unauthorized);
        }
        let secret = browser_cookie(&parts.headers).ok_or_else(ApiError::unauthorized)?;
        let digest = super::store::browser_session_digest(secret)?;
        let csrf = csrf_token(secret)?;
        // A request that changes something must also carry the token only the Hub's own page
        // can read; a cookie alone, as another site's form would send, is refused.
        let reads = matches!(
            parts.method,
            axum::http::Method::GET | axum::http::Method::HEAD
        );
        let named = parts.headers.get(CSRF_HEADER).and_then(|v| v.to_str().ok());
        if !reads && named != Some(csrf.as_str()) {
            return Err(ApiError::new(StatusCode::FORBIDDEN, "csrf").with_message(
                "This change did not come from the Hub's own page; reload the page and try again",
            ));
        }
        let (store, key) = (state.store.clone(), digest.clone());
        let now = unix_now();
        let found = blocking(state, move || {
            store.browser_session(&key, now, now + SESSION_SECONDS)
        })
        .await?;
        let Some((user, expires_at)) = found else {
            return Err(ApiError::unauthorized());
        };
        if user.disabled {
            return Err(
                ApiError::new(StatusCode::FORBIDDEN, "user_disabled").with_message(DISABLED)
            );
        }
        // This sign-in outlives restarts, so leaving the allow list must end it too.
        let login = &state.login;
        let admin = login.is_admin_user(&user);
        if login.has_allow_list()
            && !admin
            && !login.is_allowed(&user.identity.provider, user.identity.user_id, &user.login)
        {
            return Err(
                ApiError::new(StatusCode::FORBIDDEN, "not_allowed").with_message(NOT_ALLOWED)
            );
        }
        Ok(Self(Human {
            key: String::new(),
            admin,
            user,
            forge_token: String::new(),
            expires_at,
            device: None,
            browser: Some(BrowserSession { digest, csrf }),
        }))
    }
}

impl HumanSession {
    pub(super) fn admin(&self) -> std::result::Result<&Human, ApiError> {
        if self.0.admin {
            Ok(&self.0)
        } else {
            Err(forbidden())
        }
    }
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/login", get(providers))
        .route("/v1/hub/login/exchange", post(exchange))
        .route("/v1/hub/login/device/poll", post(device_poll))
        .route("/v1/hub/login/{provider}", get(browser_start))
        .route("/v1/hub/login/{provider}/callback", get(callback))
        .route("/v1/hub/login/{provider}/device", post(device_start))
        .route("/v1/hub/tokens", post(create_token).get(list_tokens))
        .route("/v1/hub/tokens/{id}", delete(revoke_token))
        .route("/v1/hub/token", delete(revoke_presented_token))
        .route("/v1/hub/session", get(session).delete(logout))
        .route("/v1/hub/users", get(list_users))
        .route("/v1/hub/users/{id}/disable", post(disable))
        .route("/v1/hub/users/{id}/enable", post(enable))
        .merge(devices::routes())
}

type ApiResult<T> = std::result::Result<T, ApiError>;

fn user_json(user: &User) -> Value {
    json!({"id": user.id, "provider": user.identity.provider, "host": user.identity.host,
        "user_id": user.identity.user_id, "login": user.login})
}

impl Signed {
    fn json(&self) -> Value {
        json!({"session": self.secret, "expires_at": self.expires_at,
            "user": user_json(&self.user), "admin": self.admin})
    }
}

fn query(raw: Option<String>) -> HashMap<String, String> {
    Url::parse("http://x/")
        .map(|mut url| {
            url.set_query(raw.as_deref());
            url.query_pairs().into_owned().collect()
        })
        .unwrap_or_default()
}

fn found(location: &str, cookie: String) -> ApiResult<Response> {
    let location = HeaderValue::from_str(location).map_err(|_| ApiError::invalid())?;
    let cookie = HeaderValue::from_str(&cookie).map_err(|_| ApiError::invalid())?;
    let mut response = StatusCode::FOUND.into_response();
    response.headers_mut().insert(header::LOCATION, location);
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

fn cookie_attributes(value: &str, max_age: u32) -> String {
    format!("{COOKIE}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={max_age}")
}

/// True when the request carries exactly one login cookie and it equals `state`.
fn cookie_matches(headers: &HeaderMap, state: &str) -> bool {
    let mut found = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|cookie| cookie.trim().strip_prefix(COOKIE)?.strip_prefix('='));
    matches!((found.next(), found.next()), (Some(value), None) if value == state)
}

/// The caller's address. Behind a reverse proxy on a loopback or private peer the last
/// `X-Forwarded-For` entry (the one the proxy appended) is used; anyone else cannot choose
/// their own key.
fn client_ip(peer: Option<Extension<ConnectInfo<SocketAddr>>>, headers: &HeaderMap) -> IpAddr {
    let peer = peer.map_or(
        IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        |Extension(ConnectInfo(a))| a.ip(),
    );
    let trusted = match peer {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        IpAddr::V6(ip) => ip.is_loopback() || ip.is_unique_local(),
    };
    headers
        .get("x-forwarded-for")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next_back()?.trim().parse().ok())
        .filter(|_| trusted)
        .unwrap_or(peer)
}

fn too_many() -> ApiError {
    ApiError::new(StatusCode::TOO_MANY_REQUESTS, "too_many_logins").with_message(
        "Too many sign-ins are waiting from this address; finish one, or wait 10 minutes and try again",
    )
}

async fn browser_start(
    State(state): State<HubState>,
    Path(name): Path<String>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    let (provider, _) = state.login.provider(&name).ok_or_else(not_found)?;
    let client = client_ip(peer, &headers);
    let params = query(raw);
    let workspace = params.get("workspace").is_some_and(|value| value == "1");
    if workspace && state.workspace.is_none() {
        return Err(not_found());
    }
    let web = !workspace && params.get("web").is_some_and(|value| value == "1");
    let cli_port = if workspace || web {
        0
    } else {
        params
            .get("cli_port")
            .and_then(|port| port.parse::<u16>().ok())
            .filter(|port| *port >= 1024)
            .ok_or_else(ApiError::invalid)?
    };
    let cli_challenge = if workspace || web {
        String::new()
    } else {
        params
            .get("cli_challenge")
            .filter(|c| {
                c.len() == 43
                    && c.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            })
            .ok_or_else(ApiError::invalid)?
            .clone()
    };
    let (csrf, verifier) = (random_hex()?, random_hex()?);
    let mut url = Url::parse(&format!(
        "{}{}",
        provider.web,
        if provider.is_gitlab() {
            "/oauth/authorize"
        } else {
            "/login/oauth/authorize"
        }
    ))
    .map_err(|_| HubError::Unavailable("authorize URL".into()))?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("client_id", &provider.client_id);
        pairs.append_pair("redirect_uri", &state.login.redirect_uri(provider.name));
        if provider.is_gitlab() {
            pairs.append_pair("response_type", "code");
            pairs.append_pair("scope", "read_api");
        }
        pairs.append_pair("state", &csrf);
        pairs.append_pair("code_challenge", &pkce_challenge(&verifier));
        pairs.append_pair("code_challenge_method", "S256");
    }
    let now = unix_now();
    let login = BrowserLogin {
        provider: name,
        verifier,
        cli_port,
        cli_challenge,
        workspace,
        web,
        client,
    };
    let mut pending = state.login.pending();
    if pending.browser.count(now, |l| l.client == client) >= MAX_PENDING_PER_CLIENT {
        return Err(too_many());
    }
    pending
        .browser
        .insert(csrf.clone(), now + BROWSER_LOGIN_SECONDS, login, now);
    drop(pending);
    found(
        url.as_str(),
        cookie_attributes(&csrf, BROWSER_LOGIN_SECONDS as u32),
    )
}

async fn callback(
    State(state): State<HubState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    RawQuery(raw): RawQuery,
) -> Response {
    let mut response = match callback_inner(&state, &name, &headers, query(raw)).await {
        Ok(response) => response,
        Err(error) => error.into_response(),
    };
    // The success redirect already clears it.
    if !response.headers().contains_key(header::SET_COOKIE)
        && let Ok(clear) = HeaderValue::from_str(&cookie_attributes("", 0))
    {
        response.headers_mut().insert(header::SET_COOKIE, clear);
    }
    response
}

/// A browser sign-in that fails goes back to the CLI with the reason and the message, and a web
/// one to the Hub's page with the reason alone (anyone can forge that link, so the page words
/// the reason itself); a workspace sign-in keeps its JSON error.
fn refused_page(pending: &BrowserLogin, error: ApiError) -> ApiResult<Response> {
    if pending.workspace {
        return Err(error);
    }
    if pending.web {
        let mut query = Url::parse("http://page.invalid/").map_err(|_| ApiError::invalid())?;
        query
            .query_pairs_mut()
            .append_pair("status", "refused")
            .append_pair("reason", error.code());
        let location = format!("/ui/#/signed-in?{}", query.query().unwrap_or_default());
        return found(&location, cookie_attributes("", 0));
    }
    // The CLI's loopback hears why, stops waiting, and sends the browser to the Hub page.
    let mut query = Url::parse("http://page.invalid/").map_err(|_| ApiError::invalid())?;
    query.query_pairs_mut().append_pair("error", error.code());
    if let Some(message) = error.message() {
        query.query_pairs_mut().append_pair("message", message);
    }
    let location = format!(
        "http://127.0.0.1:{}/callback?{}",
        pending.cli_port,
        query.query().unwrap_or_default()
    );
    found(&location, cookie_attributes("", 0))
}

async fn callback_inner(
    state: &HubState,
    name: &str,
    headers: &HeaderMap,
    params: HashMap<String, String>,
) -> ApiResult<Response> {
    let (provider, forge) = state.login.provider(name).ok_or_else(not_found)?;
    let csrf = params.get("state").ok_or_else(expired_link)?;
    // Single use: the entry goes on any attempt.
    let pending = state.login.pending().browser.take(csrf, unix_now());
    let pending = pending
        .filter(|p| p.provider == name && cookie_matches(headers, csrf))
        .ok_or_else(expired_link)?;
    let refuse = |error: ApiError| refused_page(&pending, error);
    if let Some(error) = params.get("error") {
        return refuse(if error == "access_denied" {
            ApiError::new(StatusCode::FORBIDDEN, "login_denied").with_message(format!(
                "The sign-in was cancelled on {}; start again and choose Authorize",
                forge_name(name)
            ))
        } else {
            forge_stopped(name, "stopped the sign-in", &json!({"error": error}))
        });
    }
    let Some(code) = params.get("code") else {
        return refuse(forge_stopped(name, "sent no sign-in code", &Value::Null));
    };
    let redirect = state.login.redirect_uri(name);
    let mut form = vec![
        ("client_id", provider.client_id.as_str()),
        ("client_secret", provider.client_secret.as_str()),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("code_verifier", pending.verifier.as_str()),
    ];
    if provider.is_gitlab() {
        form.push(("grant_type", "authorization_code"));
    }
    let (_, body) = match forge.post_form(&provider.token_url(), &form).await {
        Ok(reply) => reply,
        Err(_) => {
            return refuse(forge_stopped(name, "could not be reached", &Value::Null));
        }
    };
    if !body["access_token"].is_string() {
        log_forge_error("code exchange failed", &body);
        return refuse(forge_stopped(name, "didn't accept the sign-in code", &body));
    }
    let method = if pending.workspace {
        "workspace"
    } else {
        "browser"
    };
    let signed = match sign_in(state, provider, forge, method, !pending.web, &body).await {
        Ok(signed) => signed,
        Err(error) => return refuse(error),
    };
    if pending.web {
        let secret = random_hex()?;
        let digest = super::store::browser_session_digest(&secret)?;
        let (store, owner, now) = (state.store.clone(), signed.user.id.clone(), unix_now());
        blocking(state, move || {
            store.insert_browser_session(&owner, &digest, now + SESSION_SECONDS, now)
        })
        .await?;
        let mut response = found("/ui/#home", cookie_attributes("", 0))?;
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&browser_cookie_attributes(
                &secret,
                super::store::BROWSER_SESSION_MAX_SECONDS,
            ))
            .map_err(|_| ApiError::invalid())?,
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return Ok(response);
    }
    if pending.workspace {
        // The browser receives a separate short-lived HttpOnly cookie, never the
        // CLI human bearer or an agent token. Forge credentials stay server-side.
        state
            .login
            .pending()
            .sessions
            .0
            .remove(&session_digest(&signed.secret)?);
        let workspace = state.workspace.clone().ok_or_else(not_found)?;
        let cookie = blocking(state, move || workspace.issue_session(&signed.user))
            .await
            .map_err(super::workspace::api_error)?;
        let mut response = found("/ui/#workspace", cookie_attributes("", 0))?;
        response.headers_mut().append(
            header::SET_COOKIE,
            HeaderValue::from_str(&cookie).map_err(|_| ApiError::invalid())?,
        );
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        return Ok(response);
    }
    let now = unix_now();
    let handoff = random_hex()?;
    let location = format!(
        "http://127.0.0.1:{}/callback?code={handoff}",
        pending.cli_port
    );
    let value = Handoff {
        challenge: pending.cli_challenge,
        signed,
    };
    state
        .login
        .pending()
        .handoffs
        .insert(handoff, now + HANDOFF_SECONDS, value, now);
    found(&location, cookie_attributes("", 0))
}

/// The forge's `error` code when it is a short plain word: its description or body may echo
/// secrets, so nothing else of a forge error is logged or shown.
fn safe_forge_error(body: &Value) -> Option<&str> {
    body["error"].as_str().filter(|code| {
        code.len() <= 64
            && code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    })
}

fn log_forge_error(context: &str, body: &Value) {
    if let Some(code) = safe_forge_error(body) {
        eprintln!("hub: {context}: forge error {code}");
    }
}

/// The forge's display name for a provider name.
fn forge_name(provider: &str) -> &'static str {
    if provider == "gitlab" {
        "GitLab"
    } else {
        "GitHub"
    }
}

/// "start signing in again", naming what the forge said when it gave a plain code.
fn forge_stopped(provider: &str, what: &str, body: &Value) -> ApiError {
    let code = safe_forge_error(body).map_or_else(String::new, |code| format!(" ({code})"));
    failed().with_message(format!(
        "{} {what}{code}; start signing in again",
        forge_name(provider)
    ))
}

fn expired_link() -> ApiError {
    failed().with_message(
        "This sign-in link expired or was opened in another browser; start signing in again",
    )
}

pub(super) const NOT_ALLOWED: &str =
    "This Hub only lets in people its admins have allowed; ask a Hub admin to add you";
pub(super) const DISABLED: &str =
    "Your account is turned off on this Hub; ask a Hub admin to turn it back on";

/// A public repository (the reply says `private: false`) lets in only people who can write
/// to it, since anyone can read it; a private one needs only the read the reply already proves.
/// A GitLab project needs Reporter (20) on the project or its group, so Guest and Minimal
/// Access are refused; one that is not private (public, internal or unstated) needs Developer
/// (30). Missing permissions deny. A refusal is `(visibility, role held, role needed)`.
fn missing_role(
    provider: &Provider,
    reply: &Value,
) -> Option<(&'static str, &'static str, &'static str)> {
    missing_role_on(provider.is_gitlab(), reply)
}

/// [`missing_role`] by forge: GitLab when `gitlab`, else GitHub.
fn missing_role_on(
    gitlab: bool,
    reply: &Value,
) -> Option<(&'static str, &'static str, &'static str)> {
    if gitlab {
        let level = |scope: &str| reply["permissions"][scope]["access_level"].as_i64();
        let (visibility, needs, needed) = match reply["visibility"].as_str() {
            Some("private") => ("private", "reporter", GITLAB_REPORTER),
            Some("internal") => ("internal", "developer", GITLAB_DEVELOPER),
            _ => ("public", "developer", GITLAB_DEVELOPER),
        };
        let held = level("project_access").max(level("group_access"));
        if held >= Some(needed) {
            return None;
        }
        let has = match held {
            Some(GITLAB_REPORTER) => "reporter",
            Some(15) => "planner",
            Some(10) => "guest",
            Some(5) => "minimal_access",
            _ => "none",
        };
        return Some((visibility, has, needs));
    }
    let writes = ["push", "triage", "maintain", "admin"]
        .iter()
        .any(|level| reply["permissions"][level] == json!(true));
    (reply["private"] == json!(false) && !writes).then_some(("public", "read", "triage"))
}

/// The 403 for a role that is missing or too low; names the one fix, so clients can say who
/// needs which role where.
fn insufficient_role(
    login: &str,
    repository: &str,
    (visibility, has, needs): (&str, &str, &str),
) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "insufficient_role")
        .with_message(format!(
            "{login} has {} on {visibility} repository {repository}; ask a repository owner for {} or higher",
            role_name(has),
            role_name(needs)
        ))
        .with_field("login", json!(login))
        .with_field("repository", json!(repository))
        .with_field("visibility", json!(visibility))
        .with_field("has", json!(has))
        .with_field("needs", json!(needs))
}

fn role_name(role: &str) -> &'static str {
    match role {
        "read" => "Read",
        "triage" => "Triage",
        "guest" => "Guest",
        "planner" => "Planner",
        "minimal_access" => "Minimal Access",
        "reporter" => "Reporter",
        "developer" => "Developer",
        _ => "no access",
    }
}

/// Whether the person's forge token can read a connected repository on the provider's host.
async fn reads_connected(
    state: &HubState,
    provider: &Provider,
    forge: &Forge,
    token: &str,
) -> ApiResult<bool> {
    let (store, host) = (state.store.clone(), provider.host.clone());
    let repos = blocking(state, move || {
        store.repositories_on(&host, MAX_SIGN_IN_PROBES)
    })
    .await?;
    for repo in repos {
        let url = if provider.is_gitlab() {
            format!("{}/projects/{}", provider.api, repo.repository_id)
        } else {
            format!("{}/repos/{}", provider.api, repo.full_name)
        };
        let (status, body) = forge.get_json(&url, Some(token)).await?;
        // The id guards against a name that now belongs to another repository.
        if status == 200
            && body["id"].as_i64() == Some(repo.repository_id)
            && missing_role(provider, &body).is_none()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Turn a forge token reply into a hub session for the person it belongs to.
async fn sign_in(
    state: &HubState,
    provider: &Provider,
    forge: &Forge,
    method: &'static str,
    // False for a web-page sign-in, which lives in the database instead of this process.
    remember: bool,
    grant: &Value,
) -> ApiResult<Signed> {
    let token = grant["access_token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .ok_or_else(|| forge_stopped(provider.name, "didn't accept the sign-in", grant))?;
    let no_account = || forge_stopped(provider.name, "didn't return your account", &Value::Null);
    let (status, body) = forge
        .get_json(&format!("{}/user", provider.api), Some(token))
        .await
        .map_err(|_| no_account())?;
    let login_field = if provider.is_gitlab() {
        "username"
    } else {
        "login"
    };
    let (id, login) = match (body["id"].as_i64(), body[login_field].as_str()) {
        (Some(id), Some(login)) if status == 200 && id > 0 => (id, login.to_owned()),
        _ => return Err(no_account()),
    };
    let identity = ForgeIdentity {
        provider: provider.name.into(),
        host: provider.host.clone(),
        user_id: id,
    };
    let admin = state.login.is_admin(provider.name, id, &login);
    // No user, event or session is stored for anyone refused; administrators see a bounded,
    // deduplicated refusal row.
    let candidate = User {
        id: String::new(),
        identity: identity.clone(),
        login: login.clone(),
        disabled: false,
    };
    let refuse =
        |reason: &'static str| devices::record_refusal(state, &candidate, reason, "sign_in");
    if method == "workspace" {
        let workspace = state.workspace.clone().ok_or_else(not_found)?;
        let candidate = identity.clone();
        if !blocking(state, move || workspace.allows_sign_in(&candidate)).await? {
            refuse("not_allowed").await;
            return Err(
                ApiError::new(StatusCode::FORBIDDEN, "not_allowed").with_message(NOT_ALLOWED)
            );
        }
    } else if !admin {
        if state.login.has_allow_list() && !state.login.is_allowed(provider.name, id, &login) {
            refuse("not_allowed").await;
            return Err(
                ApiError::new(StatusCode::FORBIDDEN, "not_allowed").with_message(NOT_ALLOWED)
            );
        }
        if !reads_connected(state, provider, forge, token).await? {
            refuse("no_repository_role").await;
            return Err(ApiError::new(StatusCode::FORBIDDEN, "not_allowed").with_message(format!(
                "{login} has no role on a repository connected to this Hub; ask a repository owner to add you, then sign in again"
            )));
        }
    }
    let store = state.store.clone();
    let user = blocking(state, move || {
        let user = store.upsert_user(&identity, &login)?;
        if !user.disabled {
            store.record_sign_in(
                &user.id,
                if method == "workspace" {
                    "browser"
                } else {
                    method
                },
                admin,
            )?;
        }
        Ok(user)
    })
    .await?;
    if user.disabled {
        refuse("user_disabled").await;
        return Err(ApiError::new(StatusCode::FORBIDDEN, "user_disabled").with_message(DISABLED));
    }
    let now = unix_now();
    let lifetime = grant["expires_in"]
        .as_i64()
        .filter(|seconds| *seconds > 0)
        .map_or(SESSION_SECONDS, |seconds| seconds.min(SESSION_SECONDS));
    let (secret, expires_at) = (random_hex()?, now + lifetime);
    let key = session_digest(&secret)?;
    let human = Human {
        key: key.clone(),
        user: user.clone(),
        admin,
        forge_token: token.to_owned(),
        expires_at,
        device: None,
        browser: None,
    };
    if remember {
        state
            .login
            .pending()
            .sessions
            .insert_session(key, expires_at, human, now);
    }
    Ok(Signed {
        secret,
        expires_at,
        user,
        admin,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExchangeBody {
    code: String,
    verifier: String,
    /// Also sign this computer in, returning its credential once.
    #[serde(default)]
    device: Option<devices::DeviceRequest>,
}

async fn exchange(
    State(state): State<HubState>,
    JsonBody(body): JsonBody<ExchangeBody>,
) -> ApiResult<Json<Value>> {
    let handoff = state.login.pending().handoffs.take(&body.code, unix_now());
    let signed = handoff
        .filter(|h| body.verifier.len() <= 128 && pkce_challenge(&body.verifier) == h.challenge)
        .map(|h| h.signed)
        .ok_or_else(|| {
            failed().with_message(
                "The sign-in finished, but its code expired before AgentSCM could use it; sign in again",
            )
        })?;
    Ok(Json(signed_reply(&state, &signed, body.device).await?))
}

/// The sign-in reply, with a new computer credential when the client asked for one.
async fn signed_reply(
    state: &HubState,
    signed: &Signed,
    device: Option<devices::DeviceRequest>,
) -> ApiResult<Value> {
    let mut out = signed.json();
    if let Some(device) = device {
        let issued = devices::issue(state, &signed.user, device).await?;
        let key = session_digest(&signed.secret)?;
        if let Some((_, human)) = state.login.pending().sessions.0.get_mut(&key) {
            human.device = issued["id"].as_str().map(str::to_owned);
        }
        out["device"] = issued;
    }
    Ok(out)
}

async fn device_start(
    State(state): State<HubState>,
    Path(name): Path<String>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let (provider, forge) = state.login.provider(&name).ok_or_else(not_found)?;
    let client = client_ip(peer, &headers);
    if state
        .login
        .pending()
        .device
        .count(unix_now(), |l| l.client == client)
        >= MAX_PENDING_PER_CLIENT
    {
        return Err(too_many());
    }
    let unavailable = |body: &Value| {
        // Only the forge saying the app has device sign-in off makes clients skip it; a forge
        // error or outage does not.
        if matches!(
            body["error"].as_str(),
            Some("device_flow_disabled" | "unauthorized_client")
        ) {
            state.login.pending().no_device.insert(name.clone());
        }
        ApiError::new(StatusCode::CONFLICT, "device_flow_unavailable").with_message(format!(
            "This Hub's {} app doesn't allow device sign-in; sign in through the browser instead",
            forge_name(&name)
        ))
    };
    let (url, scope) = if provider.is_gitlab() {
        (
            format!("{}/oauth/authorize_device", provider.web),
            Some("read_api"),
        )
    } else {
        (format!("{}/login/device/code", provider.web), None)
    };
    let mut form = vec![("client_id", provider.client_id.as_str())];
    form.extend(scope.map(|scope| ("scope", scope)));
    let (status, body) = forge.post_form(&url, &form).await?;
    let (Some(device_code), Some(user_code), Some(uri)) = (
        body["device_code"].as_str(),
        body["user_code"].as_str(),
        body["verification_uri"].as_str(),
    ) else {
        return Err(unavailable(&body));
    };
    if status != 200 {
        return Err(unavailable(&body));
    }
    let expires_in = body["expires_in"]
        .as_i64()
        .filter(|seconds| *seconds > 0)
        .map_or(MAX_DEVICE_SECONDS, |seconds| {
            seconds.min(MAX_DEVICE_SECONDS)
        });
    let interval = body["interval"]
        .as_i64()
        .filter(|s| (1..=60).contains(s))
        .unwrap_or(5);
    let (handle, now) = (random_hex()?, unix_now());
    state.login.pending().no_device.remove(&name);
    let login = DeviceLogin {
        client,
        provider: name,
        device_code: device_code.to_owned(),
        interval,
    };
    state
        .login
        .pending()
        .device
        .insert(handle.clone(), now + expires_in, login, now);
    Ok(Json(
        json!({"handle": handle, "user_code": user_code, "verification_uri": uri,
        "interval": interval, "expires_in": expires_in}),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PollBody {
    handle: String,
    /// Also sign this computer in once the forge approves.
    #[serde(default)]
    device: Option<devices::DeviceRequest>,
}

async fn device_poll(
    State(state): State<HubState>,
    JsonBody(body): JsonBody<PollBody>,
) -> ApiResult<Json<Value>> {
    let (name, device_code, interval) = {
        let pending = state.login.pending();
        let device = pending
            .device
            .get(&body.handle, unix_now())
            .ok_or_else(|| {
                failed().with_message("This sign-in code expired; start signing in again")
            })?;
        (
            device.provider.clone(),
            device.device_code.clone(),
            device.interval,
        )
    };
    let (provider, forge) = state.login.provider(&name).ok_or_else(failed)?;
    let form = [
        ("client_id", provider.client_id.as_str()),
        ("device_code", device_code.as_str()),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
    ];
    let (status, reply) = forge.post_form(&provider.token_url(), &form).await?;
    let remove = || state.login.pending().device.take(&body.handle, unix_now());
    if reply["access_token"].is_string() {
        remove();
        let signed = sign_in(&state, provider, forge, "device", true, &reply).await?;
        let mut out = signed_reply(&state, &signed, body.device).await?;
        out["status"] = json!("signed_in");
        return Ok(Json(out));
    }
    // No token and no error code (or a forge failure): keep the handle and let the caller retry.
    if status >= 500 || reply["error"].as_str().is_none() {
        return Err(HubError::Unavailable("forge device poll gave no usable reply".into()).into());
    }
    Ok(Json(match reply["error"].as_str() {
        Some("authorization_pending") => json!({"status": "pending", "interval": interval}),
        Some("slow_down") => {
            let interval = reply["interval"]
                .as_i64()
                .filter(|s| (1..=60).contains(s))
                .unwrap_or(interval + 5);
            if let Some((_, device)) = state.login.pending().device.0.get_mut(&body.handle) {
                device.interval = interval;
            }
            json!({"status": "slow_down", "interval": interval})
        }
        Some("expired_token") => {
            remove();
            json!({"status": "expired"})
        }
        Some("access_denied") => {
            remove();
            json!({"status": "denied"})
        }
        _ => {
            log_forge_error("device poll failed", &reply);
            remove();
            return Err(forge_stopped(&name, "stopped the device sign-in", &reply));
        }
    }))
}

/// The sign-in providers, so `hub login` can pick the only one, and those whose forge app was
/// seen refusing device sign-in, so clients go straight to the browser.
async fn providers(State(state): State<HubState>) -> Json<Value> {
    let browser_only: Vec<String> = state.login.pending().no_device.iter().cloned().collect();
    Json(json!({"providers": state.login.enabled(), "browser_only": browser_only}))
}

async fn session(HumanSession(human): HumanSession) -> Json<Value> {
    let mut reply = json!({"user": user_json(&human.user), "admin": human.admin,
        "expires_at": human.expires_at});
    if let Some(browser) = &human.browser {
        reply["csrf_token"] = json!(browser.csrf);
    }
    Json(reply)
}

async fn logout(
    State(state): State<HubState>,
    HumanSession(human): HumanSession,
) -> ApiResult<Response> {
    devices::forget_role_checks(&state.login, &human.user.id);
    let Some(browser) = human.browser else {
        state.login.pending().sessions.0.remove(&human.key);
        return Ok(StatusCode::NO_CONTENT.into_response());
    };
    let store = state.store.clone();
    blocking(&state, move || {
        store.delete_browser_session(&browser.digest)
    })
    .await?;
    let cookie = HeaderValue::from_str(&browser_cookie_attributes("", 0))
        .map_err(|_| ApiError::invalid())?;
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(header::SET_COOKIE, cookie);
    Ok(response)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewToken {
    name: String,
    repository: String,
    ttl_seconds: i64,
    #[serde(default)]
    read_only: bool,
}

async fn create_token(
    State(state): State<HubState>,
    HumanSession(human): HumanSession,
    JsonBody(body): JsonBody<NewToken>,
) -> ApiResult<Response> {
    if human.browser.is_some() {
        return Err(forbidden().with_message(
            "Agent tokens are made by the CLI or this computer's sign-in, not the web page; run agentscm client setup",
        ));
    }
    validate_agent_name(&body.name)?;
    validate_repository(&body.repository)?;
    if !(1..=MAX_AGENT_TOKEN_TTL_SECONDS).contains(&body.ttl_seconds) {
        return Err(ApiError::invalid());
    }
    let (provider, forge) = state
        .login
        .provider(&human.user.identity.provider)
        .ok_or_else(ApiError::unauthorized)?;
    let url = if provider.is_gitlab() {
        format!(
            "{}/projects/{}",
            provider.api,
            body.repository.replace('/', "%2F")
        )
    } else if body.repository.split('/').count() == 2 {
        format!("{}/repos/{}", provider.api, body.repository)
    } else {
        return Err(ApiError::invalid());
    };
    let (status, reply) = forge.get_json(&url, Some(&human.forge_token)).await?;
    // The forge's own spelling is what gets stored, whatever case was asked for.
    let canonical = reply[if provider.is_gitlab() {
        "path_with_namespace"
    } else {
        "full_name"
    }]
    .as_str();
    if status == 200
        && let Some((visibility, has, needs)) = missing_role(provider, &reply)
    {
        let repository = canonical
            .filter(|name| validate_repository(name).is_ok())
            .unwrap_or(&body.repository);
        return Err(insufficient_role(
            &human.user.login,
            repository,
            (visibility, has, needs),
        ));
    }
    let (repository_id, repository) = match (status, reply["id"].as_i64(), canonical) {
        (200, Some(id), Some(name)) if id > 0 && validate_repository(name).is_ok() => {
            (id, name.to_owned())
        }
        (300..=399 | 403 | 404, ..) => return Err(forbidden()),
        (401, ..) => return Err(ApiError::unauthorized()),
        _ => return Err(HubError::Unavailable("forge repository lookup failed".into()).into()),
    };
    // Only connected repositories get tokens, and the name must still be the connected repository.
    let (store, host, named) = (
        state.store.clone(),
        provider.host.clone(),
        repository.clone(),
    );
    match blocking(&state, move || store.repository_named(&host, &named)).await? {
        None => return Err(ApiError::new(StatusCode::FORBIDDEN, "not_connected")),
        Some(connected) if connected.repository_id != repository_id => return Err(forbidden()),
        Some(_) => {}
    }
    let store = state.store.clone();
    let (owner, name, stored) = (human.user.id, body.name.clone(), repository.clone());
    let (id, secret, expires_at) = blocking(&state, move || {
        let (id, secret, expires_at) = mint_bound_agent_token(
            &*store,
            &owner,
            &name,
            &stored,
            repository_id,
            body.ttl_seconds,
            body.read_only,
        )?;
        Ok((id, secret.expose().to_owned(), expires_at))
    })
    .await?;
    let reply = json!({"id": id, "token": secret, "name": body.name,
        "repository": repository, "repository_id": repository_id, "expires_at": expires_at,
        "read_only": body.read_only});
    Ok((StatusCode::CREATED, Json(reply)).into_response())
}

async fn list_tokens(
    State(state): State<HubState>,
    HumanSession(human): HumanSession,
) -> ApiResult<Json<Value>> {
    let store = state.store.clone();
    let tokens = blocking(&state, move || store.list_agent_tokens(&human.user.id)).await?;
    let tokens: Vec<_> = tokens
        .iter()
        .map(|t| {
            json!({"id": t.id, "name": t.name, "repository": t.repository,
            "repository_id": t.repository_id, "expires_at": t.expires_at,
            "revoked": t.revoked, "read_only": t.read_only, "created_at": t.created_at})
        })
        .collect();
    Ok(Json(json!({"tokens": tokens})))
}

async fn revoke_token(
    State(state): State<HubState>,
    HumanSession(human): HumanSession,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let store = state.store.clone();
    let workspace = state.workspace.clone();
    let revoked = blocking(&state, move || {
        let operation = || store.revoke_owned_agent_token(&human.user.id, &id);
        match workspace {
            Some(workspace) => workspace.with_identity_change(operation),
            None => operation(),
        }
    })
    .await?;
    if revoked {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(not_found())
    }
}

/// An agent token revokes itself, so undoing a setup does not need a live sign-in.
async fn revoke_presented_token(
    State(state): State<HubState>,
    Reader(principal): Reader,
) -> ApiResult<StatusCode> {
    let store = state.store.clone();
    let workspace = state.workspace.clone();
    blocking(&state, move || {
        let operation = || store.revoke_owned_agent_token(&principal.owner.id, &principal.token_id);
        match workspace {
            Some(workspace) => workspace.with_identity_change(operation),
            None => operation(),
        }
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_users(State(state): State<HubState>, human: HumanSession) -> ApiResult<Json<Value>> {
    human.admin()?;
    let store = state.store.clone();
    let users = blocking(&state, move || store.list_users()).await?;
    let users: Vec<_> = users
        .iter()
        .map(|u| {
            let mut json = user_json(u);
            json["disabled"] = json!(u.disabled);
            json
        })
        .collect();
    Ok(Json(json!({"users": users})))
}

async fn set_disabled(
    state: HubState,
    human: HumanSession,
    user: String,
    disabled: bool,
) -> ApiResult<StatusCode> {
    let actor = human.admin()?.user.id.clone();
    if disabled && actor == user {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    let target = user.clone();
    let workspace = state.workspace.clone();
    // After the actor is vetted, the store refuses only an unknown target.
    match blocking(&state, move || {
        let operation = || store.set_user_disabled_by(&actor, &target, disabled);
        match workspace {
            Some(workspace) => workspace.with_user_change(&actor, &target, disabled, operation),
            None => operation(),
        }
    })
    .await
    {
        Err(HubError::Invalid(code)) if code == "workspace_last_administrator" => {
            return Err(super::workspace::api_error(HubError::Invalid(code)));
        }
        Err(HubError::Invalid(_)) => return Err(not_found()),
        result => result?,
    }
    if disabled {
        state.login.drop_sessions_of(&user);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn disable(
    State(state): State<HubState>,
    human: HumanSession,
    Path(user): Path<String>,
) -> ApiResult<StatusCode> {
    set_disabled(state, human, user, true).await
}

async fn enable(
    State(state): State<HubState>,
    human: HumanSession,
    Path(user): Path<String>,
) -> ApiResult<StatusCode> {
    set_disabled(state, human, user, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn human(user: &str) -> Human {
        Human {
            key: String::new(),
            user: User {
                id: user.into(),
                identity: ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                login: "x".into(),
                disabled: false,
            },
            admin: false,
            forge_token: String::new(),
            expires_at: 0,
            device: None,
            browser: None,
        }
    }

    #[test]
    fn entries_expire_by_explicit_time() {
        let mut table = Table::<u8>::default();
        table.insert("a".into(), 100, 1, 0);
        assert_eq!(table.get("a", 99), Some(&1));
        assert_eq!(table.get("a", 100), None);
        assert_eq!(table.take("a", 100), None);
        assert!(table.0.is_empty(), "take removes even an expired entry");
        table.insert("b".into(), 100, 2, 0);
        assert_eq!(table.take("b", 50), Some(2));
        assert_eq!(table.take("b", 50), None);
    }

    #[test]
    fn a_full_table_drops_expired_then_the_earliest_expiring() {
        let mut table = Table::<usize>::default();
        for i in 0..MAX_ENTRIES {
            table.insert(format!("k{i}"), 1000 + i as i64, i, 0);
        }
        // Full of live entries: the earliest-expiring (k0) is evicted.
        table.insert("new".into(), 5000, 0, 10);
        assert_eq!(table.0.len(), MAX_ENTRIES);
        assert!(table.get("k0", 10).is_none() && table.get("k1", 10).is_some());
        assert!(table.get("new", 10).is_some());
        // Full with some expired: those go instead of a live entry.
        table.insert("newer".into(), 6000, 0, 1005);
        assert!(table.get("k5", 1005).is_none() && table.get("k6", 1005).is_some());
        assert!(table.get("newer", 1005).is_some() && table.get("new", 1005).is_some());
        assert!(table.0.len() < MAX_ENTRIES);
    }

    #[test]
    fn a_person_keeps_at_most_eight_live_sessions() {
        let mut sessions = Table::<Human>::default();
        for i in 0..MAX_SESSIONS_PER_USER {
            sessions.insert_session(format!("a{i}"), 100 + i as i64, human("usr_a"), 0);
        }
        sessions.insert_session("b".into(), 100, human("usr_b"), 0);
        sessions.insert_session("a8".into(), 200, human("usr_a"), 0);
        let live = |user: &str| {
            sessions
                .0
                .values()
                .filter(|(_, h)| h.user.id == user)
                .count()
        };
        assert_eq!((live("usr_a"), live("usr_b")), (MAX_SESSIONS_PER_USER, 1));
        assert!(sessions.get("a0", 0).is_none() && sessions.get("a1", 0).is_some());
        // Expired sessions do not count against the cap.
        sessions.insert_session("a9".into(), 300, human("usr_a"), 150);
        assert!(sessions.get("a1", 0).is_some() && sessions.get("a9", 150).is_some());
    }

    #[test]
    fn list_entries_match_by_login_or_id() {
        let list: Vec<_> = ["github:Alice", "gitlab:#42"]
            .iter()
            .map(|text| entry("X", text).unwrap())
            .collect();
        assert!(listed(&list, "github", 1, "ALICE"));
        assert!(!listed(&list, "gitlab", 1, "alice"));
        assert!(listed(&list, "gitlab", 42, "anyone"));
        assert!(!listed(&list, "github", 42, "anyone"));
        for bad in [
            "github:#0",
            "github:#x",
            "github:#-1",
            "github:#",
            "github:",
        ] {
            assert!(entry("X", bad).is_err(), "{bad}");
        }
    }
}
