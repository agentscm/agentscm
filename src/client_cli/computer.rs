//! This computer's Hub credential on the client: mint agent tokens without a session, renew
//! them before they end, and connect a checkout automatically. Renewal and auto-connect run as
//! their own short processes, so a hook never waits on them past its budget.
use std::path::Path;
use std::time::Duration;

use reqwest::Method;
use serde_json::{Value, json};

use agentscm::hub::forge_http;
use agentscm::hub::store::unix_now;
use agentscm::{Error, Result};

use super::{
    ConnectArgs, Minted, Preminted, Provider, connect_inner, identity_hostname, lifecycle_lock,
    minted_from, provider_name, retire_owned, revoke_minted, token_name, verify_token,
    write_owned_token,
};
use crate::client_config::{Connection, load, origin_identity, owned_token_path, update};
use crate::hub_cli::{self, HubAuth, SavedDevice, call, refusal_message};

/// A computer's agent token is renewed when less than this is left of its 24 hours.
pub(crate) const RENEW_BEFORE_SECONDS: i64 = 12 * 60 * 60;
/// A Hub that does not have a repository is not asked about it again for this long.
const NOT_CONNECTED_FOR: Duration = Duration::from_secs(60 * 60);

/// The reason and the one fix for a refusal kind, for a Hub too old to send its own message.
pub(crate) fn refusal_text(kind: &str, repo: &str, forge: &str) -> String {
    let repo = hub_cli::clean(repo);
    match kind {
        "not_connected" => {
            format!("{repo} isn't connected to this Hub yet; ask a Hub admin to connect it")
        }
        "insufficient_role" | "no_repository_role" => format!(
            "Your role on {repo} isn't enough for this Hub; ask a repository owner for more access"
        ),
        "access_check_unavailable" => format!(
            "The Hub can't check your access to {repo}; ask a Hub admin to check that repository's connection"
        ),
        "permission_unavailable" => format!(
            "{forge} didn't answer the Hub's access check; nothing changed, so try again in a minute"
        ),
        "not_allowed" => {
            "This Hub only lets in people its admins have allowed; ask a Hub admin to add you"
                .to_owned()
        }
        "user_disabled" => {
            "Your account is turned off on this Hub; ask a Hub admin to turn it back on".to_owned()
        }
        _ => "This computer isn't signed in to this Hub any more; choose Sign In in the AgentSCM menu bar app or run agentscm hub login".to_owned(),
    }
}

/// Why a mint through this computer did not happen.
#[derive(Debug)]
pub(super) enum MintError {
    /// The Hub does not have this repository.
    NotConnected(String),
    /// The Hub refused; the reason and the one fix.
    Refused(String),
    /// The Hub or the forge could not be reached or did not answer; nothing changed.
    Unavailable(String),
}

impl From<MintError> for Error {
    fn from(error: MintError) -> Self {
        match error {
            MintError::NotConnected(message)
            | MintError::Refused(message)
            | MintError::Unavailable(message) => Error::Invalid(message),
        }
    }
}

fn forge_of(provider: Option<Provider>, host: &str) -> &'static str {
    match provider {
        Some(Provider::Gitlab) => "GitLab",
        Some(Provider::Github) => "GitHub",
        None if host.contains("gitlab") => "GitLab",
        None => "GitHub",
    }
}

/// Mints with this computer's credential for `connection.hub`. None when this computer has no
/// credential there, the Hub predates computer credentials, or the Hub no longer accepts the
/// credential (it is then forgotten). `provider`, when known, must match the owner.
pub(super) async fn mint(
    connection: &Connection,
    provider: Option<Provider>,
    auto: bool,
) -> std::result::Result<Option<Minted>, MintError> {
    let local = |error: Error| MintError::Refused(error.to_string());
    let Some(device) = hub_cli::saved_device(&connection.hub).map_err(local)? else {
        return Ok(None);
    };
    let unavailable = || {
        MintError::Unavailable(format!(
            "{} could not be reached; the current connection keeps working and this is retried",
            connection.hub
        ))
    };
    let refused = |reply: &Value| {
        let kind = reply["error"].as_str().unwrap_or_default();
        let message = refusal_message(reply).unwrap_or_else(|| {
            refusal_text(
                kind,
                &connection.repo,
                forge_of(provider, &connection.origin_host),
            )
        });
        match kind {
            "not_connected" => MintError::NotConnected(message),
            "permission_unavailable" => MintError::Unavailable(message),
            _ => MintError::Refused(message),
        }
    };
    let client = forge_http::client().map_err(|_| unavailable())?;
    let status_url = format!("{}/v1/hub/device", connection.hub);
    let (status, me) = call(
        &client,
        Method::GET,
        &status_url,
        Some(&device.secret),
        None,
    )
    .await
    .map_err(|_| unavailable())?;
    match status {
        200 => {}
        // A Hub from before computer credentials.
        404 => return Ok(None),
        401 => {
            let _ = hub_cli::forget_device_if(&connection.hub, &device.secret);
            return Ok(None);
        }
        400..=499 => return Err(refused(&me)),
        _ => return Err(unavailable()),
    }
    let owner_host = me["user"]["host"].as_str().and_then(identity_hostname);
    if owner_host.as_deref() != Some(connection.origin_host.as_str())
        || provider.is_some_and(|provider| me["user"]["provider"] != provider_name(provider))
    {
        return Err(MintError::Refused(
            "This Hub sign-in is for another forge than the checkout's origin; sign in with the forge that hosts it".into(),
        ));
    }
    let body = json!({"name": token_name().map_err(local)?, "repository": connection.repo,
        "auto": auto});
    let url = format!("{}/v1/hub/device/tokens", connection.hub);
    let (status, reply) = call(
        &client,
        Method::POST,
        &url,
        Some(&device.secret),
        Some(body),
    )
    .await
    .map_err(|_| unavailable())?;
    match status {
        201 => minted_from(&reply, HubAuth::Device(device.secret))
            .map(Some)
            .map_err(|_| unavailable()),
        401 => {
            let _ = hub_cli::forget_device_if(&connection.hub, &device.secret);
            Ok(None)
        }
        400..=499 | 503 => Err(refused(&reply)),
        _ => Err(unavailable()),
    }
}

/// Checks the connection the way the next renewal will, without changing it: this computer's
/// credential is live and the Hub, asked now, would mint for the repository (a read-only token,
/// revoked at once). Ok is a sentence saying so; Err the reason and fix. None when this computer
/// has no credential for the Hub.
pub(crate) async fn check(connection: &Connection) -> Option<std::result::Result<String, String>> {
    let device = hub_cli::saved_device(&connection.hub).ok()??;
    let client = forge_http::client().ok()?;
    let hub = &connection.hub;
    let failed = |reply: &Value| {
        Err(refusal_message(reply).unwrap_or_else(|| {
            refusal_text(
                reply["error"].as_str().unwrap_or_default(),
                &connection.repo,
                forge_of(None, &connection.origin_host),
            )
        }))
    };
    let (status, me) = match call(
        &client,
        Method::GET,
        &format!("{hub}/v1/hub/device"),
        Some(&device.secret),
        None,
    )
    .await
    {
        Ok(reply) => reply,
        Err(_) => {
            return Some(Err(format!(
                "{hub} could not be reached; check again in a minute"
            )));
        }
    };
    if status != 200 {
        return Some(failed(&me));
    }
    let body = json!({"name": "agentscm-doctor", "repository": connection.repo,
        "read_only": true});
    let url = format!("{hub}/v1/hub/device/tokens");
    let (status, reply) = match call(
        &client,
        Method::POST,
        &url,
        Some(&device.secret),
        Some(body),
    )
    .await
    {
        Ok(reply) => reply,
        Err(_) => {
            return Some(Err(format!(
                "{hub} could not be reached; check again in a minute"
            )));
        }
    };
    if status != 201 {
        return Some(failed(&reply));
    }
    if let Some(id) = reply["id"].as_str() {
        let url = HubAuth::Device(device.secret.clone()).revoke_url(hub, id);
        let _ = call(&client, Method::DELETE, &url, Some(&device.secret), None).await;
    }
    Some(Ok(format!(
        "Connection verified: {} is signed in to {hub} as {}, and your role on {} allows coordination now.",
        hub_cli::clean(&device.name),
        hub_cli::clean(me["user"]["login"].as_str().unwrap_or("?")),
        hub_cli::clean(&connection.repo)
    )))
}

/// Gives this computer a credential from a live session, for sign-ins made before computers
/// had one. Best effort and silent; does nothing when it already has one or no session lives.
pub(crate) async fn enroll(hub: &str) {
    if !matches!(hub_cli::saved_device(hub), Ok(None)) {
        return;
    }
    let Ok((saved, session)) = hub_cli::load_session(Some(hub)) else {
        return;
    };
    let Ok(client) = forge_http::client() else {
        return;
    };
    let mut body = json!({});
    if let Some(name) = hub_cli::computer_name() {
        body["name"] = json!(name);
    }
    let url = format!("{saved}/v1/hub/devices");
    if let Ok((201, reply)) = call(&client, Method::POST, &url, Some(&session), Some(body)).await
        && let Some(device) = SavedDevice::from_reply(&reply["device"])
        && !hub_cli::save_device(&saved, &session, device.clone()).unwrap_or(false)
    {
        // Another sign-in won the race: this credential would never be used.
        let url = format!("{saved}/v1/hub/device");
        let _ = call(&client, Method::DELETE, &url, Some(&device.secret), None).await;
    }
}

/// A refusal that needs the person, kept per checkout for the hook and the menu bar app:
/// `$XDG_CONFIG_HOME/agentscm/attention/<hash of the root>.json`, owner-only.
fn attention_path(root: &Path) -> Result<std::path::PathBuf> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(root.as_os_str().as_encoded_bytes());
    Ok(hub_cli::config_dir()?
        .join("agentscm")
        .join("attention")
        .join(format!("{digest:x}.json")))
}

fn record_attention(root: &Path, message: Option<&str>) {
    let Ok(path) = attention_path(root) else {
        return;
    };
    match message {
        Some(message) => {
            let text = json!({"root": root, "message": message, "at": unix_now()}).to_string();
            let _ = hub_cli::write_private(&path, &text);
        }
        None => {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// The recorded refusal for the checkout at `root`, if any.
pub(crate) fn attention(root: &Path) -> Option<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(attention_path(root).ok()?)
        .ok()?
        .take(16 * 1024)
        .read_to_string(&mut text)
        .ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    value["message"]
        .as_str()
        .filter(|message| !message.chars().any(char::is_control))
        .map(str::to_owned)
}

/// True when the checkout's own managed token ends within [`RENEW_BEFORE_SECONDS`].
pub(crate) fn due(connection: &Connection, now: i64) -> bool {
    connection.owned_token_id.is_some()
        && connection
            .owned_token_expires_at
            .is_some_and(|expires| expires - now < RENEW_BEFORE_SECONDS)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Renewal {
    NotDue,
    Renewed,
    /// The Hub refused; the reason and fix.
    Refused(String),
    /// Try again later: the Hub or forge did not answer, or the connection changed meanwhile.
    Unavailable,
}

/// Renews the checkout's managed token when it is due: mint a new one, save it, then revoke
/// the old one, so a failure part way leaves a working token.
async fn renew(root: &Path) -> Result<Renewal> {
    let Some(saved) = load()?
        .into_iter()
        .find(|connection| connection.root == root)
    else {
        return Ok(Renewal::NotDue);
    };
    if !due(&saved, unix_now()) {
        return Ok(Renewal::NotDue);
    }
    enroll(&saved.hub).await;
    let minted = match mint(&saved, None, false).await {
        Ok(Some(minted)) => minted,
        // Still signed in only through a session (an older Hub): that path renews nothing.
        Ok(None) if hub_cli::is_signed_in(Some(&saved.hub)).unwrap_or(false) => {
            return Ok(Renewal::Unavailable);
        }
        Ok(None) => {
            return Ok(Renewal::Refused(refusal_text(
                "sign_in_required",
                &saved.repo,
                "",
            )));
        }
        Err(MintError::Unavailable(_)) => return Ok(Renewal::Unavailable),
        Err(MintError::NotConnected(message) | MintError::Refused(message)) => {
            return Ok(Renewal::Refused(message));
        }
    };
    let verified = match verify_token(&saved, &minted.token).await {
        Ok(verified) => verified,
        Err(_) => {
            revoke_minted(&saved, &minted).await;
            return Ok(Renewal::Unavailable);
        }
    };
    let mut renewed = saved.clone();
    renewed.token_file = owned_token_path(&saved, &minted.id)?;
    renewed.owned_token_id = Some(minted.id.clone());
    renewed.owned_token_expires_at = Some(verified.expires_at);
    let lifecycle = lifecycle_lock()?;
    let unchanged = load()?
        .into_iter()
        .find(|connection| connection.root == root)
        .is_some_and(|latest| latest == saved);
    let saved_new = unchanged
        && write_owned_token(&renewed.token_file, &minted.token).is_ok()
        && match update(root, Some(&saved), Some(renewed.clone())) {
            Ok(()) => true,
            Err(_) => {
                let _ = std::fs::remove_file(&renewed.token_file);
                false
            }
        };
    drop(lifecycle);
    if !saved_new {
        revoke_minted(&saved, &minted).await;
        return Ok(Renewal::Unavailable);
    }
    retire_owned(&saved, true).await;
    Ok(Renewal::Renewed)
}

/// `agentscm client renew`: prints one JSON line, `{"result": "renewed" | "not_due" |
/// "unavailable" | "refused", "message"?}`.
pub(crate) async fn renew_command(root: &Path) -> Result<()> {
    let renewal = renew(root).await?;
    match &renewal {
        Renewal::Renewed => record_attention(root, None),
        Renewal::Refused(message) => record_attention(root, Some(message)),
        Renewal::NotDue | Renewal::Unavailable => {}
    }
    let line = match renewal {
        Renewal::NotDue => json!({"result": "not_due"}),
        Renewal::Renewed => json!({"result": "renewed"}),
        Renewal::Unavailable => json!({"result": "unavailable"}),
        Renewal::Refused(message) => json!({"result": "refused", "message": message}),
    };
    println!("{line}");
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AutoConnect {
    /// Connected to this repository.
    Connected(String),
    /// A Hub that has the repository refused; the reason and fix.
    Refused(String),
    /// No Hub with auto-connect on has this repository; a fork's hint when a Hub gave one.
    NotConnected(Option<String>),
    /// Auto-connect is off everywhere, the checkout has no usable origin, or a Hub did not
    /// answer.
    Skipped,
}

fn not_connected_key(hub: &str, host: &str, repo: &str) -> String {
    format!(
        "not-connected\0{hub}\0{host}\0{}",
        repo.to_ascii_lowercase()
    )
}

/// Connects the checkout at `root` through the first Hub with auto-connect on that has its
/// repository. Only this computer's credential is used, nothing is printed, no sign-in starts
/// and no file in the checkout is written; the connect path keeps its program fence,
/// lifecycle lock and origin revalidation.
async fn auto_connect(root: &Path) -> AutoConnect {
    let Ok(hubs) = hub_cli::auto_connect_hubs() else {
        return AutoConnect::Skipped;
    };
    let (Ok((host, repo)), Ok(config)) = (origin_identity(root), hub_cli::config_dir()) else {
        return AutoConnect::Skipped;
    };
    let mut not_connected = None;
    for (hub, _) in hubs {
        let key = not_connected_key(&hub, &host, &repo);
        if crate::hook_cli::marked_within(&config, &key, NOT_CONNECTED_FOR) {
            not_connected.get_or_insert(None);
            continue;
        }
        let probe = Connection {
            root: root.to_owned(),
            origin_host: host.clone(),
            hub: hub.clone(),
            repo: repo.clone(),
            token_file: Default::default(),
            report_completion: false,
            owned_token_id: None,
            owned_token_expires_at: None,
            agents: Vec::new(),
        };
        let minted = match mint(&probe, None, true).await {
            Ok(Some(minted)) => minted,
            Ok(None) | Err(MintError::Unavailable(_)) => continue,
            Err(MintError::NotConnected(message)) => {
                crate::hook_cli::mark(&config, &key);
                // A Hub that recognised a fork says which repository to use instead.
                let hint = message.contains(" fork of ").then_some(message);
                not_connected = Some(not_connected.flatten().or(hint));
                continue;
            }
            Err(MintError::Refused(message)) => return AutoConnect::Refused(message),
        };
        let (id, auth) = (minted.id.clone(), minted.auth.clone());
        let args = ConnectArgs {
            hub: Some(hub.clone()),
            no_sign_in: true,
            auto: true,
            minted: Some(Preminted(minted)),
            ..ConnectArgs::default()
        };
        let result = connect_inner(args, root.to_owned(), None, None).await;
        let kept = load().ok().is_some_and(|saved| {
            saved
                .iter()
                .any(|connection| connection.owned_token_id.as_deref() == Some(id.as_str()))
        });
        if !kept {
            // Connect revokes what it minted when it fails, but not a token it never used.
            let unused = Minted {
                id,
                token: String::new(),
                auth,
            };
            revoke_minted(&probe, &unused).await;
        }
        return match result {
            Ok(()) => AutoConnect::Connected(repo),
            Err(Error::Invalid(message)) => AutoConnect::Refused(message),
            Err(error) => AutoConnect::Refused(error.to_string()),
        };
    }
    match not_connected {
        Some(hint) => AutoConnect::NotConnected(hint),
        None => AutoConnect::Skipped,
    }
}

/// `agentscm client auto-connect`: prints one JSON line, `{"result": "connected" |
/// "refused" | "not_connected" | "skipped", "repository"?, "message"?}`.
pub(crate) async fn auto_connect_command(root: &Path) -> Result<()> {
    let result = auto_connect(root).await;
    match &result {
        AutoConnect::Connected(_) => record_attention(root, None),
        AutoConnect::Refused(message) | AutoConnect::NotConnected(Some(message)) => {
            record_attention(root, Some(message));
        }
        AutoConnect::NotConnected(None) | AutoConnect::Skipped => {}
    }
    let line = match result {
        AutoConnect::Connected(repo) => json!({"result": "connected", "repository": repo}),
        AutoConnect::Refused(message) => json!({"result": "refused", "message": message}),
        AutoConnect::NotConnected(hint) => json!({"result": "not_connected", "message": hint}),
        AutoConnect::Skipped => json!({"result": "skipped"}),
    };
    println!("{line}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection(id: Option<&str>, expires: Option<i64>) -> Connection {
        Connection {
            root: "/work/widgets".into(),
            origin_host: "github.com".into(),
            hub: "https://hub.example".into(),
            repo: "acme/widgets".into(),
            token_file: "/config/token".into(),
            report_completion: false,
            owned_token_id: id.map(str::to_owned),
            owned_token_expires_at: expires,
            agents: Vec::new(),
        }
    }

    #[test]
    fn only_a_managed_token_near_its_end_is_due() {
        let now = 1_000_000;
        let id = Some("tok_0123");
        assert!(!due(
            &connection(id, Some(now + RENEW_BEFORE_SECONDS + 1)),
            now
        ));
        assert!(due(
            &connection(id, Some(now + RENEW_BEFORE_SECONDS - 1)),
            now
        ));
        assert!(due(&connection(id, Some(now - 10)), now));
        // A supplied token's lifecycle is managed elsewhere.
        assert!(!due(&connection(None, None), now));
    }

    #[test]
    fn every_fallback_refusal_has_a_reason_and_a_fix() {
        for kind in [
            "not_connected",
            "insufficient_role",
            "access_check_unavailable",
            "permission_unavailable",
            "not_allowed",
            "user_disabled",
            "sign_in_required",
        ] {
            let text = refusal_text(kind, "acme/widgets", "GitHub");
            assert!(text.contains("; "), "{kind} has no fix: {text}");
        }
    }
}
