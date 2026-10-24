//! Prepare a short-lived, read-only Hub URL for the native tray.

use std::fmt;
use std::path::PathBuf;

use reqwest::Method;
use serde_json::{Value, json};

use agentscm::hub::forge_http::{self, check_base_url};

use crate::client_config;
use crate::client_status::{BindingState, ConnectionStatus, HubCheck};

const TOKEN_TTL_SECONDS: i64 = 30 * 60;
const EXPIRY_SLOP_SECONDS: i64 = 60;
const MIN_OPEN_LIFETIME_SECONDS: i64 = 60;
const TOKEN_NAME: &str = "agentscm-tray";

/// The checkout identity selected by the menu snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct OpenRequest {
    root: PathBuf,
    hub: String,
    repository: String,
    origin_host: String,
    provider: Option<String>,
    generation: String,
}

impl OpenRequest {
    pub(super) fn from_status(connection: &ConnectionStatus) -> Result<Self, OpenError> {
        if !matches!(&connection.binding.state, BindingState::Current) {
            return Err(OpenError::InvalidConnection);
        }
        let generation = connection
            .binding
            .activity_generation
            .clone()
            .ok_or(OpenError::InvalidConnection)?;
        let hub = check_base_url(&connection.hub).map_err(|_| OpenError::InvalidConnection)?;
        if hub != connection.hub
            || agentscm::hub::store::validate_repository(&connection.repository).is_err()
        {
            return Err(OpenError::InvalidConnection);
        }
        let saved = client_config::load()
            .map_err(|_| OpenError::InvalidConnection)?
            .into_iter()
            .find(|saved| saved.root == connection.root)
            .ok_or(OpenError::InvalidConnection)?;
        if saved.hub != hub || saved.repo != connection.repository {
            return Err(OpenError::InvalidConnection);
        }
        let provider = expected_provider(&saved.origin_host, &connection.hub_check);
        let request = Self {
            root: connection.root.clone(),
            hub,
            repository: connection.repository.clone(),
            origin_host: saved.origin_host,
            provider,
            generation,
        };
        validate_current(&request)?;
        Ok(request)
    }
}

/// A secret-bearing URL ready for the native browser-opening boundary.
///
/// Deliberately has no `Debug`, `Display`, or `Clone` implementation. The URL
/// fragment and saved human session must not reach logs or process arguments.
pub(super) struct PreparedOpen {
    request: OpenRequest,
    url: String,
    hub: String,
    token_id: String,
    token_secret: String,
    human_session: String,
    expires_at: i64,
    client: reqwest::Client,
}

impl PreparedOpen {
    /// Recheck immediately before handing the URL to `NSWorkspace`.
    pub(super) fn validate_current(&self) -> Result<(), OpenError> {
        self.validate_current_at(agentscm::hub::store::unix_now())
    }

    /// Expose the secret URL only to the native browser-opening boundary.
    pub(super) fn url(&self) -> &str {
        &self.url
    }

    /// Consume a prepared URL that will not be opened and revoke its token.
    pub(super) async fn revoke(self) {
        revoke_token(
            &self.client,
            &self.hub,
            Some(&self.token_secret),
            Some(&self.token_id),
            &self.human_session,
        )
        .await;
    }

    fn validate_current_at(&self, now: i64) -> Result<(), OpenError> {
        validate_current(&self.request)?;
        validate_session(&self.hub, &self.human_session)?;
        if !prepared_lifetime_valid(self.expires_at, now) {
            return Err(OpenError::PreparedExpired);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OpenError {
    InvalidConnection,
    MissingSignIn,
    RejectedSignIn,
    SignInChanged,
    RepositoryNotConnected,
    AccessDenied,
    PreparedExpired,
    Unavailable,
}

impl fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidConnection => "the checkout connection changed; refresh and try again",
            Self::MissingSignIn => {
                "no saved sign-in for this Hub; open this checkout's Troubleshooting → Hub Sign-in, copy the command for your forge, and run it in Terminal"
            }
            Self::RejectedSignIn => {
                "the saved Hub sign-in is no longer accepted; open this checkout's Troubleshooting → Hub Sign-in, copy the command for your forge, and run it in Terminal"
            }
            Self::SignInChanged => {
                "the saved Hub sign-in changed; try Open Hub again to use the current sign-in"
            }
            Self::RepositoryNotConnected => {
                "that repository is not connected to this Hub; ask its operator to connect it, then try again"
            }
            Self::AccessDenied => {
                "this sign-in cannot open that repository; check Hub repository access"
            }
            Self::PreparedExpired => {
                "the read-only Hub session expired before it could open; try again"
            }
            Self::Unavailable => "the Hub is unavailable; try again",
        })
    }
}

impl std::error::Error for OpenError {}

/// Mint and verify a short-lived browser token for the selected checkout.
pub(super) async fn prepare_open(request: OpenRequest) -> Result<PreparedOpen, OpenError> {
    validate_current(&request)?;
    let (hub, human_session) = load_exact_session(&request.hub)?;
    let client = forge_http::client().map_err(|_| OpenError::Unavailable)?;
    let minted = mint_and_confirm(
        &client,
        &hub,
        &human_session,
        &request.repository,
        &request.origin_host,
        request.provider.as_deref(),
        agentscm::hub::store::unix_now(),
    )
    .await?;

    if let Err(error) =
        validate_current(&request).and_then(|()| validate_session(&hub, &human_session))
    {
        revoke_token(
            &client,
            &hub,
            Some(&minted.secret),
            Some(&minted.id),
            &human_session,
        )
        .await;
        return Err(error);
    }
    let url = match browser_url(&hub, &minted.secret) {
        Ok(url) => url,
        Err(error) => {
            revoke_token(
                &client,
                &hub,
                Some(&minted.secret),
                Some(&minted.id),
                &human_session,
            )
            .await;
            return Err(error);
        }
    };
    Ok(PreparedOpen {
        request,
        url,
        hub,
        token_id: minted.id,
        token_secret: minted.secret,
        human_session,
        expires_at: minted.expires_at,
        client,
    })
}

fn load_exact_session(expected_hub: &str) -> Result<(String, String), OpenError> {
    match crate::hub_cli::load_session(Some(expected_hub)) {
        Ok((hub, session)) if hub == expected_hub => Ok((hub, session)),
        Ok(_) => Err(OpenError::InvalidConnection),
        Err(agentscm::Error::Invalid(message))
            if message == "not signed in; run agentscm hub login"
                || message == "no saved session for this hub; run agentscm hub login" =>
        {
            Err(OpenError::MissingSignIn)
        }
        Err(_) => Err(OpenError::Unavailable),
    }
}

fn validate_current(expected: &OpenRequest) -> Result<(), OpenError> {
    let connections = client_config::load().map_err(|_| OpenError::InvalidConnection)?;
    let connection = connections
        .iter()
        .find(|connection| connection.root == expected.root)
        .ok_or(OpenError::InvalidConnection)?;
    if connection.hub != expected.hub
        || connection.repo != expected.repository
        || connection.origin_host != expected.origin_host
        || !client_config::is_current(connection)
    {
        return Err(OpenError::InvalidConnection);
    }
    let token = crate::client_cli::read_token(&connection.token_file)
        .map_err(|_| OpenError::InvalidConnection)?;
    if crate::client_activity::activity_binding(connection, &token) != expected.generation {
        return Err(OpenError::InvalidConnection);
    }
    Ok(())
}

fn validate_session(hub: &str, expected_session: &str) -> Result<(), OpenError> {
    match crate::hub_cli::load_session(Some(hub)) {
        Ok((selected_hub, session))
            if same_session(hub, expected_session, Some((&selected_hub, &session))) =>
        {
            Ok(())
        }
        Ok(_) => Err(OpenError::SignInChanged),
        Err(agentscm::Error::Invalid(message))
            if message == "not signed in; run agentscm hub login"
                || message == "no saved session for this hub; run agentscm hub login" =>
        {
            Err(OpenError::SignInChanged)
        }
        Err(_) => Err(OpenError::Unavailable),
    }
}

fn same_session(hub: &str, expected: &str, selected: Option<(&str, &str)>) -> bool {
    selected.is_some_and(|(selected_hub, selected)| selected_hub == hub && selected == expected)
}

fn prepared_lifetime_valid(expires_at: i64, now: i64) -> bool {
    expires_at > now + MIN_OPEN_LIFETIME_SECONDS
}

fn expected_provider(origin_host: &str, check: &HubCheck) -> Option<String> {
    match origin_host {
        "github.com" => Some("github".into()),
        "gitlab.com" => Some("gitlab".into()),
        _ => match check {
            HubCheck::Valid { provider, host, .. }
                if identity_host(host).as_deref() == Some(origin_host) =>
            {
                Some(provider.clone())
            }
            _ => None,
        },
    }
}

fn identity_host(host: &str) -> Option<String> {
    reqwest::Url::parse(&format!("https://{host}"))
        .ok()?
        .host_str()
        .map(|host| host.trim_matches(['[', ']']).to_ascii_lowercase())
}

struct MintedToken {
    id: String,
    secret: String,
    repository_id: i64,
    expires_at: i64,
}

async fn mint_and_confirm(
    client: &reqwest::Client,
    hub: &str,
    human_session: &str,
    repository: &str,
    origin_host: &str,
    provider: Option<&str>,
    now: i64,
) -> Result<MintedToken, OpenError> {
    let body = json!({
        "name": TOKEN_NAME,
        "repository": repository,
        "ttl_seconds": TOKEN_TTL_SECONDS,
        "read_only": true,
    });
    let (status, reply) = crate::hub_cli::call(
        client,
        Method::POST,
        &format!("{hub}/v1/hub/tokens"),
        Some(human_session),
        Some(body),
    )
    .await
    .map_err(|_| OpenError::Unavailable)?;
    match status {
        401 => return Err(OpenError::RejectedSignIn),
        403 if reply["error"] == "not_connected" => {
            return Err(OpenError::RepositoryNotConnected);
        }
        403 => return Err(OpenError::AccessDenied),
        201 => {}
        _ => return Err(OpenError::Unavailable),
    }
    let cleanup_secret = reply["token"]
        .as_str()
        .filter(|token| valid_token_secret(token));
    let cleanup_id = reply["id"].as_str().filter(|id| valid_token_id(id));
    let minted = match validate_mint(&reply, repository, now) {
        Ok(minted) => minted,
        Err(error) => {
            revoke_token(client, hub, cleanup_secret, cleanup_id, human_session).await;
            return Err(error);
        }
    };

    let confirmed = crate::hub_cli::call(
        client,
        Method::GET,
        &format!("{hub}/v1/hub/whoami"),
        Some(&minted.secret),
        None,
    )
    .await;
    let confirmed = match confirmed {
        Ok((200, reply)) => {
            validate_confirmation(&reply, repository, origin_host, provider, now, &minted)
        }
        _ => Err(OpenError::Unavailable),
    };
    if let Err(error) = confirmed {
        revoke_token(
            client,
            hub,
            Some(&minted.secret),
            Some(&minted.id),
            human_session,
        )
        .await;
        return Err(error);
    }
    Ok(minted)
}

struct MintFields<'a> {
    id: &'a str,
    repository: &'a str,
    repository_id: i64,
    expires_at: i64,
    read_only: bool,
}

fn mint_fields(reply: &Value) -> Option<MintFields<'_>> {
    Some(MintFields {
        id: reply["id"].as_str()?,
        repository: reply["repository"].as_str()?,
        repository_id: reply["repository_id"].as_i64()?,
        expires_at: reply["expires_at"].as_i64()?,
        read_only: reply["read_only"].as_bool()?,
    })
}

fn validate_mint(reply: &Value, repository: &str, now: i64) -> Result<MintedToken, OpenError> {
    let fields = mint_fields(reply).ok_or(OpenError::Unavailable)?;
    let secret = reply["token"].as_str().ok_or(OpenError::Unavailable)?;
    if !valid_token_id(fields.id)
        || !valid_token_secret(secret)
        || !fields.repository.eq_ignore_ascii_case(repository)
        || fields.repository_id <= 0
        || !fields.read_only
        || !valid_expiry(fields.expires_at, now)
    {
        return Err(OpenError::Unavailable);
    }
    Ok(MintedToken {
        id: fields.id.to_owned(),
        secret: secret.to_owned(),
        repository_id: fields.repository_id,
        expires_at: fields.expires_at,
    })
}

fn validate_confirmation(
    reply: &Value,
    repository: &str,
    origin_host: &str,
    provider: Option<&str>,
    now: i64,
    minted: &MintedToken,
) -> Result<(), OpenError> {
    let token = &reply["token"];
    let fields = mint_fields(token).ok_or(OpenError::Unavailable)?;
    let confirmed_host = reply["owner"]["host"]
        .as_str()
        .and_then(identity_host)
        .ok_or(OpenError::Unavailable)?;
    let confirmed_provider = reply["owner"]["provider"]
        .as_str()
        .ok_or(OpenError::Unavailable)?;
    if fields.id != minted.id
        || !fields.repository.eq_ignore_ascii_case(repository)
        || fields.repository_id != minted.repository_id
        || fields.expires_at != minted.expires_at
        || !fields.read_only
        || !valid_expiry(fields.expires_at, now)
        || confirmed_host != origin_host
        || provider.is_some_and(|provider| confirmed_provider != provider)
    {
        return Err(OpenError::Unavailable);
    }
    Ok(())
}

fn valid_expiry(expires_at: i64, now: i64) -> bool {
    let expected = now + TOKEN_TTL_SECONDS;
    (expected - EXPIRY_SLOP_SECONDS..=expected + EXPIRY_SLOP_SECONDS).contains(&expires_at)
}

fn valid_token_id(id: &str) -> bool {
    id.len() == 36
        && id.starts_with("tok_")
        && id[4..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_token_secret(secret: &str) -> bool {
    secret.len() == 64
        && secret
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn browser_url(hub: &str, token: &str) -> Result<String, OpenError> {
    if !valid_token_secret(token) {
        return Err(OpenError::Unavailable);
    }
    let mut url =
        reqwest::Url::parse(&format!("{hub}/ui/")).map_err(|_| OpenError::InvalidConnection)?;
    url.set_fragment(Some(&format!("token={token}")));
    Ok(url.to_string())
}

async fn revoke_token(
    client: &reqwest::Client,
    hub: &str,
    token_secret: Option<&str>,
    token_id: Option<&str>,
    human_session: &str,
) {
    if let Some(token) = token_secret.filter(|token| valid_token_secret(token)) {
        let target = format!("{hub}/v1/hub/token");
        if let Ok((204 | 401, _)) =
            crate::hub_cli::call(client, Method::DELETE, &target, Some(token), None).await
        {
            return;
        }
    }
    let Some(token_id) = token_id.filter(|id| valid_token_id(id)) else {
        return;
    };
    let _ = crate::hub_cli::call(
        client,
        Method::DELETE,
        &format!("{hub}/v1/hub/tokens/{token_id}"),
        Some(human_session),
        None,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    const ID: &str = "tok_0123456789abcdef0123456789abcdef";
    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn mint(expires_at: i64) -> Value {
        json!({
            "id": ID,
            "token": SECRET,
            "name": TOKEN_NAME,
            "repository": "acme/widgets",
            "repository_id": 7,
            "expires_at": expires_at,
            "read_only": true,
        })
    }

    fn confirmation(expires_at: i64, repository: &str, read_only: bool) -> Value {
        json!({
            "token": {
                "id": ID,
                "name": TOKEN_NAME,
                "repository": repository,
                "repository_id": 7,
                "expires_at": expires_at,
                "read_only": read_only,
            },
            "owner": {
                "provider": "github",
                "host": "github.com",
                "user_id": 42,
                "login": "alice",
            },
        })
    }

    fn fake_hub(responses: Vec<(u16, Value)>) -> (String, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            for (status, response) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                let expected = loop {
                    let mut chunk = [0_u8; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0, "request ended before its headers");
                    request.extend_from_slice(&chunk[..count]);
                    if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..end]);
                        let length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        break end + 4 + length;
                    }
                };
                while request.len() < expected {
                    let mut chunk = [0_u8; 4096];
                    let count = stream.read(&mut chunk).unwrap();
                    assert!(count > 0, "request ended before its body");
                    request.extend_from_slice(&chunk[..count]);
                }
                sender.send(String::from_utf8(request).unwrap()).unwrap();
                let body = if status == 204 {
                    String::new()
                } else {
                    response.to_string()
                };
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
        });
        (format!("http://{address}"), receiver)
    }

    fn receive_requests(receiver: &mpsc::Receiver<String>, count: usize) -> Vec<String> {
        (0..count)
            .map(|_| receiver.recv_timeout(Duration::from_secs(2)).unwrap())
            .collect()
    }

    async fn mint_from_hub(
        client: &reqwest::Client,
        hub: &str,
        now: i64,
    ) -> Result<MintedToken, OpenError> {
        mint_and_confirm(
            client,
            hub,
            "human-session",
            "acme/widgets",
            "github.com",
            Some("github"),
            now,
        )
        .await
    }

    fn assert_self_revoke(request: &str) {
        assert!(request.starts_with("DELETE /v1/hub/token HTTP/1.1"));
        assert!(request.contains(&format!("authorization: Bearer {SECRET}")));
        assert!(!request.contains("human-session"));
    }

    fn assert_human_revoke(request: &str) {
        assert!(request.starts_with(&format!("DELETE /v1/hub/tokens/{ID} HTTP/1.1")));
        assert!(request.contains("authorization: Bearer human-session"));
        assert!(!request.contains(SECRET));
    }

    #[tokio::test]
    async fn mint_and_confirm_uses_the_read_only_contract() {
        let now = 1_000;
        let expiry = now + TOKEN_TTL_SECONDS;
        let (hub, requests) = fake_hub(vec![
            (201, mint(expiry)),
            (200, confirmation(expiry, "Acme/Widgets", true)),
        ]);
        let client = forge_http::client().unwrap();
        let minted = mint_from_hub(&client, &hub, now).await.unwrap();
        assert_eq!(minted.id, ID);
        assert_eq!(minted.secret, SECRET);

        let requests = receive_requests(&requests, 2);
        let (headers, body) = requests[0].split_once("\r\n\r\n").unwrap();
        assert!(headers.starts_with("POST /v1/hub/tokens HTTP/1.1"));
        assert!(headers.contains("authorization: Bearer human-session"));
        assert!(!body.contains("human-session"));
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(
            body,
            json!({
                "name": TOKEN_NAME,
                "repository": "acme/widgets",
                "ttl_seconds": TOKEN_TTL_SECONDS,
                "read_only": true,
            })
        );
        assert!(requests[1].starts_with("GET /v1/hub/whoami HTTP/1.1"));
        assert!(requests[1].contains(&format!("authorization: Bearer {SECRET}")));
        assert!(!requests[1].contains("human-session"));
    }

    #[tokio::test]
    async fn mint_reports_a_rejected_human_session_without_claiming_expiry() {
        let (hub, requests) = fake_hub(vec![(401, json!({"error": "unauthorized"}))]);
        let client = forge_http::client().unwrap();
        let error = mint_from_hub(&client, &hub, 1_000).await.err().unwrap();
        assert_eq!(error, OpenError::RejectedSignIn);
        let requests = receive_requests(&requests, 1);
        assert!(requests[0].starts_with("POST /v1/hub/tokens HTTP/1.1"));
    }

    #[tokio::test]
    async fn mint_reports_safe_repository_access_failures() {
        for (reply, expected) in [
            (
                json!({"error": "not_connected"}),
                OpenError::RepositoryNotConnected,
            ),
            (
                json!({"error": "server-private-material"}),
                OpenError::AccessDenied,
            ),
        ] {
            let (hub, requests) = fake_hub(vec![(403, reply)]);
            let client = forge_http::client().unwrap();
            let error = mint_from_hub(&client, &hub, 1_000).await.err().unwrap();
            assert_eq!(error, expected);
            assert!(!error.to_string().contains("server-private-material"));
            let requests = receive_requests(&requests, 1);
            assert!(requests[0].starts_with("POST /v1/hub/tokens HTTP/1.1"));
        }
    }

    #[tokio::test]
    async fn bad_mint_read_only_claim_is_revoked() {
        let now = 1_000;
        let mut bad = mint(now + TOKEN_TTL_SECONDS);
        bad["read_only"] = json!(false);
        let (hub, requests) = fake_hub(vec![(201, bad), (204, Value::Null)]);
        let client = forge_http::client().unwrap();
        let error = mint_from_hub(&client, &hub, now).await.err().unwrap();
        assert_eq!(error, OpenError::Unavailable);
        let requests = receive_requests(&requests, 2);
        assert_self_revoke(&requests[1]);
    }

    #[tokio::test]
    async fn malformed_mint_id_with_a_valid_secret_self_revokes() {
        let now = 1_000;
        let mut bad = mint(now + TOKEN_TTL_SECONDS);
        bad["id"] = json!("malformed-id");
        let (hub, requests) = fake_hub(vec![(201, bad), (204, Value::Null)]);
        let client = forge_http::client().unwrap();
        let error = mint_from_hub(&client, &hub, now).await.err().unwrap();
        assert_eq!(error, OpenError::Unavailable);
        let requests = receive_requests(&requests, 2);
        assert_self_revoke(&requests[1]);
    }

    #[tokio::test]
    async fn self_revoke_does_not_depend_on_the_human_session() {
        let (hub, requests) = fake_hub(vec![(204, Value::Null)]);
        let client = forge_http::client().unwrap();
        revoke_token(
            &client,
            &hub,
            Some(SECRET),
            Some(ID),
            "rejected-human-session",
        )
        .await;
        let requests = receive_requests(&requests, 1);
        assert_self_revoke(&requests[0]);
        assert!(!requests[0].contains("rejected-human-session"));
    }

    #[tokio::test]
    async fn wrong_confirmation_repository_or_capability_is_revoked() {
        let now = 1_000;
        let expiry = now + TOKEN_TTL_SECONDS;
        for confirmation in [
            confirmation(expiry, "other/widgets", true),
            confirmation(expiry, "acme/widgets", false),
        ] {
            let (hub, requests) = fake_hub(vec![
                (201, mint(expiry)),
                (200, confirmation),
                (204, Value::Null),
            ]);
            let client = forge_http::client().unwrap();
            let error = mint_from_hub(&client, &hub, now).await.err().unwrap();
            assert_eq!(error, OpenError::Unavailable);
            let requests = receive_requests(&requests, 3);
            assert!(requests[1].starts_with("GET /v1/hub/whoami HTTP/1.1"));
            assert_self_revoke(&requests[2]);
        }
    }

    #[tokio::test]
    async fn failed_confirmation_is_revoked() {
        let now = 1_000;
        let expiry = now + TOKEN_TTL_SECONDS;
        let (hub, requests) = fake_hub(vec![
            (201, mint(expiry)),
            (503, json!({"error": "unavailable"})),
            (503, json!({"error": "unavailable"})),
            (204, Value::Null),
        ]);
        let client = forge_http::client().unwrap();
        let error = mint_from_hub(&client, &hub, now).await.err().unwrap();
        assert_eq!(error, OpenError::Unavailable);
        let requests = receive_requests(&requests, 4);
        assert!(requests[1].starts_with("GET /v1/hub/whoami HTTP/1.1"));
        assert_self_revoke(&requests[2]);
        assert_human_revoke(&requests[3]);
    }

    #[test]
    fn browser_url_keeps_the_secret_only_in_the_fragment() {
        let value = browser_url("https://hub.example/base", SECRET).unwrap();
        let parsed = reqwest::Url::parse(&value).unwrap();
        assert_eq!(
            parsed.as_str(),
            format!("https://hub.example/base/ui/#token={SECRET}")
        );
        assert!(parsed.query().is_none());
        assert!(!parsed.path().contains(SECRET));
        assert_eq!(parsed.fragment(), Some(format!("token={SECRET}").as_str()));
    }

    #[test]
    fn responses_must_confirm_read_only_repository_identity_and_expiry() {
        let now = 1_000;
        let minted = validate_mint(&mint(now + TOKEN_TTL_SECONDS), "acme/widgets", now).unwrap();
        let mut confirmed = json!({"token": {
            "id": ID,
            "repository": "acme/widgets",
            "repository_id": 7,
            "expires_at": now + TOKEN_TTL_SECONDS,
            "read_only": true,
        }});
        confirmed["owner"] = json!({"provider": "github", "host": "github.com"});
        validate_confirmation(
            &confirmed,
            "acme/widgets",
            "github.com",
            Some("github"),
            now,
            &minted,
        )
        .unwrap();

        for bad in [
            json!({"id": ID, "token": SECRET, "repository": "other/widgets",
                "repository_id": 7,
                "expires_at": now + TOKEN_TTL_SECONDS,
                "read_only": true}),
            json!({"id": ID, "token": SECRET, "repository": "acme/widgets",
                "repository_id": 7, "expires_at": now + 10, "read_only": false}),
            json!({"id": ID, "token": SECRET, "repository": "acme/widgets",
                "repository_id": 7, "expires_at": now + TOKEN_TTL_SECONDS + 61,
                "read_only": true}),
            json!({"id": ID, "token": SECRET, "repository": "acme/widgets",
                "repository_id": 7,
                "expires_at": now + TOKEN_TTL_SECONDS - EXPIRY_SLOP_SECONDS - 1,
                "read_only": true}),
        ] {
            assert_eq!(
                validate_mint(&bad, "acme/widgets", now).err().unwrap(),
                OpenError::Unavailable
            );
        }
    }

    #[test]
    fn confirmation_rejects_same_named_repository_on_another_forge() {
        let now = 1_000;
        let expiry = now + TOKEN_TTL_SECONDS;
        let minted = validate_mint(&mint(expiry), "acme/widgets", now).unwrap();
        let mut wrong_host = confirmation(expiry, "acme/widgets", true);
        wrong_host["owner"]["host"] = json!("gitlab.com");
        let mut wrong_provider = confirmation(expiry, "acme/widgets", true);
        wrong_provider["owner"]["provider"] = json!("gitlab");
        for reply in [wrong_host, wrong_provider] {
            assert_eq!(
                validate_confirmation(
                    &reply,
                    "acme/widgets",
                    "github.com",
                    Some("github"),
                    now,
                    &minted,
                )
                .err()
                .unwrap(),
                OpenError::Unavailable
            );
        }
    }

    #[test]
    fn saved_session_identity_and_prepared_expiry_are_fenced() {
        let hub = "https://hub.example";
        assert!(same_session(
            hub,
            "expected-session",
            Some((hub, "expected-session"))
        ));
        assert!(!same_session(
            hub,
            "expected-session",
            Some((hub, "replacement-session"))
        ));
        assert!(!same_session(hub, "expected-session", None));

        assert!(!prepared_lifetime_valid(1_060, 1_000));
        assert!(prepared_lifetime_valid(1_061, 1_000));
        assert!(!prepared_lifetime_valid(999, 1_000));
    }

    #[test]
    fn errors_never_echo_server_or_secret_material() {
        let secret = "server-private-material";
        let reply = json!({"error": secret, "token": secret});
        let error = validate_mint(&reply, "acme/widgets", 1_000).err().unwrap();
        assert!(!error.to_string().contains(secret));
        assert!(!OpenError::InvalidConnection.to_string().contains(SECRET));
        assert!(!OpenError::MissingSignIn.to_string().contains(SECRET));
        assert!(!OpenError::RejectedSignIn.to_string().contains(SECRET));
        assert!(!OpenError::AccessDenied.to_string().contains(SECRET));
        assert!(!OpenError::Unavailable.to_string().contains(SECRET));
    }
}
