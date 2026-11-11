//! Connected apps. An operator registers an external app in `AGENTSCM_HUB_APPS_FILE`; a person
//! signs in to the Hub's web pages and approves it (OAuth 2.0 authorization code with PKCE S256),
//! and the app receives a revocable credential bound to that person: a computer row with
//! `app_id` set. The credential reads what the person's computer reads and mints read-only
//! tokens only; on the person routes it answers coordination and file requests for the person,
//! recorded with the app's `via`, and lists and answers the permission prompts and questions of
//! the person's agents. Every other route refuses it.
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use axum::Router;
use axum::extract::{FromRequest, FromRequestParts, RawQuery, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json};
use reqwest::Url;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::super::device::validate_device_name;
use super::super::handoff::Via;
use super::super::http::ApiError;
use super::super::store::{User, unix_now};
use super::super::sync::escape;
use super::super::{HubError, HubState, Result};
use super::devices::{self, DeviceRequest};
use super::{ApiResult, CSRF_HEADER, HumanSession, Login, not_found, pkce_challenge, random_hex};
use crate::hmac::constant_time_eq;

pub const APPS_FILE_ENV: &str = "AGENTSCM_HUB_APPS_FILE";
const AUTHORIZE: &str = "/v1/hub/apps/authorize";
const MAX_APPS: usize = 16;
const MAX_FILE_BYTES: u64 = 64 * 1024;
const MIN_SECRET_BYTES: usize = 32;
const CODE_SECONDS: i64 = 60;
/// Codes one person may have waiting, so one person cannot fill the table.
const MAX_CODES_PER_USER: usize = 8;
const MAX_STATE_BYTES: usize = 512;
/// The longest approval URL a sign-in returns to.
const MAX_RETURN_BYTES: usize = 2048;

/// A registered app. Only a digest of its client secret is kept.
#[derive(Debug)]
pub(super) struct App {
    pub(super) id: String,
    pub(super) name: String,
    redirect_uri: String,
    secret: [u8; 32],
    pub(super) via: Via,
    /// How the Hub names the app's answers to people, as in "via Slack".
    via_label: &'static str,
}

impl App {
    fn secret_matches(&self, presented: &str) -> bool {
        constant_time_eq(&Sha256::digest(presented.as_bytes()), &self.secret)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppsFile {
    apps: Vec<AppEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AppEntry {
    id: String,
    name: String,
    redirect_uri: String,
    secret_file: PathBuf,
    via: String,
}

/// The apps in the file `AGENTSCM_HUB_APPS_FILE` names, under the Slack config's file rules: an
/// absolute path to a regular file, not a symlink, owned by this user and closed to group and
/// others. Unset means no apps.
pub(super) fn load(lookup: &impl Fn(&str) -> Option<String>) -> Result<Vec<App>> {
    let Some(path) = lookup(APPS_FILE_ENV) else {
        return Ok(Vec::new());
    };
    let bad = |why: String| HubError::Invalid(format!("{APPS_FILE_ENV} {why}"));
    if cfg!(not(unix)) {
        return Err(bad("needs Unix private-file permissions".into()));
    }
    let bytes = super::super::slack::private_file(Path::new(&path), MAX_FILE_BYTES).map_err(|_| {
        bad("must be an absolute path to a regular file of at most 64 KiB, owned by this user and not readable by group or others".into())
    })?;
    let file: AppsFile = serde_json::from_slice(&bytes).map_err(|_| {
        bad(r#"must be {"apps": [{"id", "name", "redirect_uri", "secret_file", "via"}]}"#.into())
    })?;
    if file.apps.len() > MAX_APPS {
        return Err(bad(format!("lists more than {MAX_APPS} apps")));
    }
    let mut apps: Vec<App> = Vec::new();
    for entry in file.apps {
        let id = entry.id;
        if !valid_id(&id) || apps.iter().any(|app| app.id == id) {
            return Err(bad(format!(
                "app ids must be unique, 3 to 64 of a-z, 0-9 and -: {id:?}"
            )));
        }
        if validate_device_name(&entry.name).is_err() {
            return Err(bad(format!(
                "app {id}: name must be 1 to 64 printable characters"
            )));
        }
        if !valid_redirect(&entry.redirect_uri) {
            return Err(bad(format!(
                "app {id}: redirect_uri must be an exact https URL (http only on a loopback IP), without a query or fragment"
            )));
        }
        let (via, via_label) = match entry.via.as_str() {
            "slack" => (Via::Slack, "Slack"),
            _ => return Err(bad(format!("app {id}: via must be \"slack\""))),
        };
        let secret = super::super::slack::secret(&entry.secret_file)
            .ok()
            .filter(|secret| secret.len() >= MIN_SECRET_BYTES)
            .ok_or_else(|| {
                bad(format!(
                    "app {id}: secret_file must be a private file holding at least {MIN_SECRET_BYTES} bytes"
                ))
            })?;
        apps.push(App {
            id,
            name: entry.name,
            redirect_uri: entry.redirect_uri,
            secret: Sha256::digest(secret.as_bytes()).into(),
            via,
            via_label,
        });
    }
    Ok(apps)
}

fn valid_id(id: &str) -> bool {
    (3..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// Written exactly as the URL parser writes it back, so the registered string is the one compared.
fn valid_redirect(uri: &str) -> bool {
    let Ok(url) = Url::parse(uri) else {
        return false;
    };
    let loopback = url
        .host_str()
        .and_then(|host| host.trim_matches(['[', ']']).parse::<IpAddr>().ok())
        .is_some_and(|ip| ip.is_loopback());
    (url.scheme() == "https" || (url.scheme() == "http" && loopback))
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.as_str() == uri
}

/// The approval page as a web sign-in's `return_to`: that one path with a form-encoded query.
pub(super) fn return_path(path: &str) -> bool {
    path.len() <= MAX_RETURN_BYTES
        && path
            .strip_prefix(AUTHORIZE)
            .and_then(|rest| rest.strip_prefix('?'))
            .is_some_and(|query| {
                query
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"*-._~%&=+".contains(&b))
            })
}

/// An approved request, waiting for the app to exchange it.
pub(super) struct Code {
    client_id: String,
    redirect_uri: String,
    challenge: String,
    user: User,
}

pub(super) fn routes() -> Router<HubState> {
    Router::new()
        .route(AUTHORIZE, get(authorize).post(decide))
        .route("/v1/hub/apps/token", post(token))
}

/// A valid authorization request: a registered app, its exact redirect, a state and PKCE S256.
struct Grant<'a> {
    app: &'a App,
    state: String,
    challenge: String,
}

impl Grant<'_> {
    /// The request's own parameters, form-encoded.
    fn query(&self) -> ApiResult<String> {
        let mut url = Url::parse("http://page.invalid/").map_err(|_| ApiError::invalid())?;
        url.query_pairs_mut()
            .append_pair("client_id", &self.app.id)
            .append_pair("redirect_uri", &self.app.redirect_uri)
            .append_pair("state", &self.state)
            .append_pair("code_challenge", &self.challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.query().unwrap_or_default().to_owned())
    }

    fn back(&self, pairs: &[(&str, &str)]) -> ApiResult<Response> {
        let mut to = Url::parse(&self.app.redirect_uri).map_err(|_| ApiError::invalid())?;
        to.query_pairs_mut()
            .extend_pairs(pairs)
            .append_pair("state", &self.state);
        let mut response = StatusCode::SEE_OTHER.into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::LOCATION,
            HeaderValue::from_str(to.as_str()).map_err(|_| ApiError::invalid())?,
        );
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        headers.insert(
            header::REFERRER_POLICY,
            HeaderValue::from_static("no-referrer"),
        );
        Ok(response)
    }
}

fn grant<'a>(
    login: &'a Login,
    params: &HashMap<String, String>,
) -> std::result::Result<Grant<'a>, &'static str> {
    let field = |name: &str| params.get(name).map_or("", String::as_str);
    let app = login
        .app(field("client_id"))
        .ok_or("This app is not registered on this Hub.")?;
    if field("redirect_uri") != app.redirect_uri {
        return Err("This app's return address is not the one registered on this Hub.");
    }
    let state = field("state");
    if state.is_empty()
        || state.len() > MAX_STATE_BYTES
        || !state.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err("This app sent an invalid sign-in request (state).");
    }
    let challenge = field("code_challenge");
    if field("code_challenge_method") != "S256"
        || challenge.len() != 43
        || !challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err("This app sent an invalid sign-in request (PKCE S256 is required).");
    }
    Ok(Grant {
        app,
        state: state.to_owned(),
        challenge: challenge.to_owned(),
    })
}

/// A server-rendered page: no script, framed by nobody, never stored. Forms may post only here,
/// and `form_to` is the one other origin the approval form's redirect may reach.
fn page(status: StatusCode, title: &str, body: &str, form_to: Option<&str>) -> Response {
    let html = format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{}</title><style>body{{font:16px/1.5 system-ui,sans-serif;max-width:34rem;\
         margin:2rem auto;padding:0 1rem;color:#222;background:#fff}}button,a.button{{font:inherit;\
         padding:.5rem 1rem;margin-right:.5rem}}</style></head><body><main>{body}</main></body></html>",
        escape(title)
    );
    let csp = format!(
        "default-src 'none'; style-src 'unsafe-inline'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'{}",
        form_to
            .map(|origin| format!(" {origin}"))
            .unwrap_or_default()
    );
    let mut response = (status, Html(html)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(csp) = HeaderValue::from_str(&csp) {
        headers.insert(header::CONTENT_SECURITY_POLICY, csp);
    }
    response
}

/// An error page; it never redirects, so a bad request cannot send anyone anywhere.
fn refused(status: StatusCode, message: &str) -> Response {
    let body = format!(
        "<h1>This app can't connect</h1><p>{}</p><p><a href=\"/ui/\">Open the Hub</a></p>",
        escape(message)
    );
    page(status, "Can't connect", &body, None)
}

async fn authorize(
    State(state): State<HubState>,
    session: std::result::Result<HumanSession, ApiError>,
    RawQuery(raw): RawQuery,
) -> ApiResult<Response> {
    if state.login.config.apps.is_empty() {
        return Err(not_found());
    }
    let params = super::query(raw);
    let grant = match grant(&state.login, &params) {
        Ok(grant) => grant,
        Err(why) => return Ok(refused(StatusCode::BAD_REQUEST, why)),
    };
    let name = escape(&grant.app.name);
    let signed_in = session.and_then(|session| {
        let csrf = session.0.browser.as_ref().map(|b| b.csrf.clone());
        session
            .web_user()
            .map(|user| (user.clone(), csrf.unwrap_or_default()))
    });
    let (user, csrf) = match signed_in {
        Ok(signed_in) => signed_in,
        Err(error) if error.code() == "unauthorized" => {
            // The web sign-in cookie is SameSite=Strict, so a browser arriving from the app's
            // site (or from the forge) does not send it. The first visit reloads this page from
            // the Hub itself, which does; only then is the person sent to sign in.
            let here = format!("{AUTHORIZE}?{}", grant.query()?);
            let mut links = String::new();
            for provider in state.login.enabled() {
                let mut url =
                    Url::parse("http://page.invalid/").map_err(|_| ApiError::invalid())?;
                url.query_pairs_mut()
                    .append_pair("web", "1")
                    .append_pair("return_to", &here);
                links.push_str(&format!(
                    "<p><a class=\"button\" href=\"{}\">Sign in with {}</a></p>",
                    escape(&format!(
                        "/v1/hub/login/{provider}?{}",
                        url.query().unwrap_or_default()
                    )),
                    super::forge_name(provider)
                ));
            }
            let hop = escape(&format!("{here}&hop=1"));
            let reload = if params.get("hop").is_some_and(|hop| hop == "1") {
                String::new()
            } else {
                // The link is for a browser that ignores the refresh.
                format!(
                    "<meta http-equiv=\"refresh\" content=\"0;url={hop}\">\
                     <p><a href=\"{hop}\">Continue</a></p>"
                )
            };
            let body = format!(
                "{reload}<h1>Sign in to connect {name}</h1>\
                 <p>Sign in to this Hub, then choose whether {name} may act for you.</p>{links}"
            );
            return Ok(page(StatusCode::OK, "Sign in", &body, None));
        }
        Err(error) => {
            let message = error.message().unwrap_or(
                "Your sign-in can't approve apps; sign in to the Hub's web pages again.",
            );
            return Ok(refused(StatusCode::FORBIDDEN, message));
        }
    };
    let hidden: String = [
        ("csrf", csrf.as_str()),
        ("client_id", grant.app.id.as_str()),
        ("redirect_uri", grant.app.redirect_uri.as_str()),
        ("state", grant.state.as_str()),
        ("code_challenge", grant.challenge.as_str()),
        ("code_challenge_method", "S256"),
    ]
    .iter()
    .map(|(field, value)| {
        format!(
            "<input type=\"hidden\" name=\"{field}\" value=\"{}\">",
            escape(value)
        )
    })
    .collect();
    let via = grant.app.via_label;
    let body = format!(
        "<h1>Connect {name}?</h1>\
         <p>You are signed in to this Hub as <strong>{login}</strong>.</p>\
         <p>{name} will be able to:</p><ul>\
         <li>read your work across the repositories you can read on this Hub;</li>\
         <li>answer coordination requests and file requests for you, shown as \u{201c}via {via}\u{201d};</li>\
         <li>answer your agents' permission prompts and questions in those repositories: allow or deny what an agent asks to run, \
         pick one of its \u{201c}don't ask again\u{201d} choices, and send it the reason or answer you give.</li></ul>\
         <p>It can't edit code, start your agents, or change Work.</p>\
         <p>You can revoke it at any time under Computers.</p>\
         <form method=\"post\" action=\"{AUTHORIZE}\">{hidden}\
         <button type=\"submit\" name=\"decision\" value=\"allow\">Allow</button>\
         <button type=\"submit\" name=\"decision\" value=\"deny\">Deny</button></form>",
        login = escape(&user.login),
    );
    let origin = Url::parse(&grant.app.redirect_uri)
        .map(|url| url.origin().ascii_serialization())
        .map_err(|_| ApiError::invalid())?;
    Ok(page(
        StatusCode::OK,
        &format!("Connect {}", grant.app.name),
        &body,
        Some(&origin),
    ))
}

/// Allow or Deny from the approval page. The form's CSRF field stands in for the header the Hub's
/// own pages send, so every check of the web sign-in applies to it.
async fn decide(
    State(state): State<HubState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> ApiResult<Response> {
    if state.login.config.apps.is_empty() {
        return Err(not_found());
    }
    let grant = match grant(&state.login, &form) {
        Ok(grant) => grant,
        Err(why) => return Ok(refused(StatusCode::BAD_REQUEST, why)),
    };
    let (mut parts, ()) = Request::new(()).into_parts();
    parts.method = Method::POST;
    parts.headers = headers;
    parts.headers.remove(CSRF_HEADER);
    if let Some(csrf) = form
        .get("csrf")
        .and_then(|csrf| HeaderValue::from_str(csrf).ok())
    {
        parts.headers.insert(CSRF_HEADER, csrf);
    }
    let user = match HumanSession::from_request_parts(&mut parts, &state)
        .await
        .and_then(|session| session.web_user().cloned())
    {
        Ok(user) => user,
        Err(error) => {
            let message = error
                .message()
                .unwrap_or("Your Hub sign-in ended; start connecting from the app again.");
            return Ok(refused(StatusCode::FORBIDDEN, message));
        }
    };
    match form.get("decision").map(String::as_str) {
        Some("deny") => grant.back(&[("error", "access_denied")]),
        Some("allow") => {
            let (code, now) = (random_hex()?, unix_now());
            let mut pending = state.login.pending();
            if pending.app_codes.count(now, |c| c.user.id == user.id) >= MAX_CODES_PER_USER {
                drop(pending);
                return Ok(refused(
                    StatusCode::TOO_MANY_REQUESTS,
                    "Too many connections are waiting; wait a minute and try again.",
                ));
            }
            let waiting = Code {
                client_id: grant.app.id.clone(),
                redirect_uri: grant.app.redirect_uri.clone(),
                challenge: grant.challenge.clone(),
                user,
            };
            pending
                .app_codes
                .insert(code.clone(), now + CODE_SECONDS, waiting, now);
            drop(pending);
            grant.back(&[("code", &code)])
        }
        _ => Ok(refused(StatusCode::BAD_REQUEST, "Choose Allow or Deny.")),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TokenRequest {
    grant_type: String,
    code: String,
    redirect_uri: String,
    client_id: String,
    client_secret: String,
    code_verifier: String,
}

fn oauth(code: &'static str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, code)
}

/// RFC 7636: 43 to 128 unreserved characters.
fn valid_verifier(verifier: &str) -> bool {
    (43..=128).contains(&verifier.len())
        && verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._~".contains(&b))
}

/// The app exchanges a code for the person's credential. The code is spent only by the app it
/// was issued to (checked under the same lock), so another app cannot use it up; it goes on any
/// use by its own app after that.
async fn token(State(state): State<HubState>, request: Request) -> ApiResult<Response> {
    if state.login.config.apps.is_empty() {
        return Err(not_found());
    }
    let json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    let body = if json {
        Json::<TokenRequest>::from_request(request, &state)
            .await
            .map(|Json(body)| body)
            .ok()
    } else {
        Form::<TokenRequest>::from_request(request, &state)
            .await
            .map(|Form(body)| body)
            .ok()
    };
    let body = body
        .filter(|body| body.grant_type == "authorization_code")
        .ok_or_else(|| oauth("invalid_request"))?;
    let app = state
        .login
        .app(&body.client_id)
        .filter(|app| app.secret_matches(&body.client_secret))
        .ok_or_else(|| oauth("invalid_client"))?;
    // Spent under one lock, and only by the app it was issued to.
    let code = state
        .login
        .pending()
        .app_codes
        .take_if(&body.code, unix_now(), |code| code.client_id == app.id);
    let code = code
        .filter(|code| {
            code.redirect_uri == body.redirect_uri
                && valid_verifier(&body.code_verifier)
                && constant_time_eq(
                    pkce_challenge(&body.code_verifier).as_bytes(),
                    code.challenge.as_bytes(),
                )
        })
        .ok_or_else(|| oauth("invalid_grant"))?;
    let request = DeviceRequest {
        name: Some(app.name.clone()),
        replaces: None,
    };
    let issued = devices::issue(&state, &code.user, request, Some(&app.id))
        .await
        .map_err(|error| match error.code() {
            // The person was disabled after approving.
            "invalid_request" => oauth("invalid_grant"),
            _ => error,
        })?;
    let identity = &code.user.identity;
    let reply = json!({"credential": issued["secret"],
        "device": {"id": issued["id"], "name": issued["name"]},
        "owner": {"provider": identity.provider, "host": identity.host,
            "user_id": identity.user_id, "login": code.user.login}});
    let mut response = Json(reply).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

/// The person behind a person route: the web sign-in with its CSRF token (`via` hub), or a
/// connected app's credential (the app's `via`). Any other credential is refused as before.
pub(in crate::hub) struct Person {
    pub(in crate::hub) user: User,
    pub(in crate::hub) via: Via,
}

impl FromRequestParts<HubState> for Person {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &HubState,
    ) -> std::result::Result<Self, ApiError> {
        if parts.headers.contains_key(header::AUTHORIZATION)
            && let Some((user, via)) = devices::app_credential(parts, state).await?
        {
            // A write as the person, so leaving the allow list stops it, as for the web sign-in.
            devices::allow_listed(state, &user, "app_answer").await?;
            return Ok(Self { user, via });
        }
        let human = HumanSession::from_request_parts(parts, state).await?;
        Ok(Self {
            user: human.web_user()?.clone(),
            via: Via::Hub,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_redirects_are_strict() {
        for ok in ["abc", "example-app", &"a".repeat(64)] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["ab", "Example", "a_b", "a.b", &"a".repeat(65)] {
            assert!(!valid_id(bad), "{bad}");
        }
        for ok in [
            "https://app.example.com/callback",
            "http://127.0.0.1:8080/cb",
            "http://[::1]/cb",
        ] {
            assert!(valid_redirect(ok), "{ok}");
        }
        for bad in [
            "http://app.example.com/callback",
            "http://localhost/cb",
            "https://app.example.com",
            "https://App.example.com/callback",
            "https://app.example.com/callback?x=1",
            "https://app.example.com/callback#x",
            "https://user@app.example.com/callback",
            "javascript:alert(1)",
            "/callback",
        ] {
            assert!(!valid_redirect(bad), "{bad}");
        }
    }

    #[test]
    fn a_sign_in_returns_only_to_the_approval_page() {
        assert!(return_path(
            "/v1/hub/apps/authorize?client_id=a&state=b%2Fc+d"
        ));
        for bad in [
            "/v1/hub/apps/authorize",
            "/v1/hub/apps/authorizex?a=b",
            "/v1/hub/apps/token?a=b",
            "/v1/hub/apps/authorize?a=b/../x",
            "/v1/hub/apps/authorize?a=b#x",
            "//evil.example/v1/hub/apps/authorize?a=b",
            "/v1/hub/apps/authorize?a=<b>",
        ] {
            assert!(!return_path(bad), "{bad}");
        }
        assert!(!return_path(&format!(
            "/v1/hub/apps/authorize?a={}",
            "b".repeat(MAX_RETURN_BYTES)
        )));
    }

    #[test]
    fn verifiers_follow_rfc_7636() {
        assert!(valid_verifier(&"a".repeat(43)));
        assert!(valid_verifier(&format!("{}-._~", "a".repeat(124))));
        assert!(!valid_verifier(&"a".repeat(42)));
        assert!(!valid_verifier(&"a".repeat(129)));
        assert!(!valid_verifier(&format!("{}+", "a".repeat(43))));
    }
}
