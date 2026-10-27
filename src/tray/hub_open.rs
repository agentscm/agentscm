//! Prepare a short-lived, read-only Hub URL for the native tray, when this computer's own Hub page
//! (`agentscm open`) cannot start.

use std::path::PathBuf;

use agentscm::hub::forge_http::{self, check_base_url};

use crate::client_config;
use crate::client_status::{BindingState, ConnectionStatus};
use crate::hub_cli::HubAuth;
pub(super) use crate::hub_read_token::OpenError;
use crate::hub_read_token::{
    expected_provider, load_exact_session, mint_and_confirm, revoke_token, valid_token_secret,
};

const MIN_OPEN_LIFETIME_SECONDS: i64 = 60;
/// The name the tray gives the tokens it mints; the local Hub page uses its own.
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
    /// The checkout this request opens.
    pub(super) fn root(&self) -> &std::path::Path {
        &self.root
    }

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
    human_session: HubAuth,
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

/// Mint and verify a short-lived browser token for the selected checkout.
pub(super) async fn prepare_open(request: OpenRequest) -> Result<PreparedOpen, OpenError> {
    validate_current(&request)?;
    crate::client_cli::enroll(&request.hub).await;
    let (hub, human_session) = load_exact_session(&request.hub)?;
    let client = forge_http::client().map_err(|_| OpenError::Unavailable)?;
    let minted = mint_and_confirm(
        &client,
        &hub,
        &human_session,
        &crate::hub_read_token::TokenRequest {
            repository: &request.repository,
            origin_host: &request.origin_host,
            provider: request.provider.as_deref(),
            name: TOKEN_NAME,
        },
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

fn validate_session(hub: &str, expected_session: &HubAuth) -> Result<(), OpenError> {
    match crate::hub_cli::load_auth(Some(hub)) {
        Ok((selected_hub, session))
            if same_session(hub, expected_session, Some((&selected_hub, &session))) =>
        {
            Ok(())
        }
        Ok(_) => Err(OpenError::SignInChanged),
        Err(agentscm::Error::Invalid(message))
            if message == "not signed in; run agentscm hub login"
                || message == "no saved session for this hub; run agentscm hub login"
                || message == crate::hub_cli::SESSION_EXPIRED =>
        {
            Err(OpenError::SignInChanged)
        }
        Err(_) => Err(OpenError::Unavailable),
    }
}

fn same_session(hub: &str, expected: &HubAuth, selected: Option<(&str, &HubAuth)>) -> bool {
    selected.is_some_and(|(selected_hub, selected)| selected_hub == hub && selected == expected)
}

fn prepared_lifetime_valid(expires_at: i64, now: i64) -> bool {
    expires_at > now + MIN_OPEN_LIFETIME_SECONDS
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

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

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
    fn saved_session_identity_and_prepared_expiry_are_fenced() {
        let hub = "https://hub.example";
        let expected = HubAuth::Session("expected-session".into());
        assert!(same_session(hub, &expected, Some((hub, &expected))));
        let replacement = HubAuth::Session("replacement-session".into());
        assert!(!same_session(hub, &expected, Some((hub, &replacement))));
        // A computer credential that replaced the session is a changed sign-in too.
        let device = HubAuth::Device("expected-session".into());
        assert!(!same_session(hub, &expected, Some((hub, &device))));
        assert!(!same_session(hub, &expected, None));

        assert!(!prepared_lifetime_valid(1_060, 1_000));
        assert!(prepared_lifetime_valid(1_061, 1_000));
        assert!(!prepared_lifetime_valid(999, 1_000));
    }
}
