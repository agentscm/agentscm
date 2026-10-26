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
    ConnectArgs, Minted, Provider, connect_inner, identity_hostname, invalid, lifecycle_lock,
    minted_from, provider_name, retire_owned, revoke_minted, token_name, verify_token,
    write_owned_token,
};
use crate::client_config::{Connection, load, origin_identity, owned_token_path, update};
use crate::hub_cli::{self, HubAuth, SavedDevice, call, map_hub_error};

pub(super) const NO_CREDENTIAL: &str = "this computer has no Hub credential for auto-connect";
/// A computer's agent token is renewed when less than this is left of its 24 hours.
pub(crate) const RENEW_BEFORE_SECONDS: i64 = 12 * 60 * 60;
/// A Hub that does not have a repository is not asked about it again for this long.
const NOT_CONNECTED_FOR: Duration = Duration::from_secs(60 * 60);

/// The reason and the one fix for a refusal kind from the Hub, as people and agents read it.
pub(crate) fn refusal_text(kind: &str, repo: &str, hub: &str) -> String {
    let repo = hub_cli::clean(repo);
    match kind {
        "not_connected" => {
            format!("{repo} isn't connected to {hub}; ask a Hub admin to connect it")
        }
        "no_repository_role" => format!(
            "your forge role on {repo} isn't enough for {hub} (a public repository needs triage or more, a private one read; on GitLab Developer or Reporter); ask a repository admin for access"
        ),
        "access_check_unavailable" => format!(
            "{hub} can't check your access to {repo}; ask the Hub admin to check its connection's credentials"
        ),
        "permission_unavailable" => format!(
            "the forge didn't answer {hub}'s access check for {repo}; the current connection keeps working and this is retried"
        ),
        "not_allowed" => format!("you're not on {hub}'s list of allowed users; ask a Hub admin"),
        "user_disabled" => format!("your account on {hub} is disabled; ask a Hub admin"),
        _ => format!(
            "this computer is signed out of {hub}; sign in again from the AgentSCM menu bar app or with `agentscm hub login`"
        ),
    }
}

/// Mints with this computer's credential for `connection.hub`. None when this computer has no
/// credential there, or the Hub no longer accepts it (it is then forgotten). A refusal is an
/// error with its reason and fix. `provider`, when known, must match the credential's owner.
pub(super) async fn mint(
    connection: &Connection,
    provider: Option<Provider>,
    auto: bool,
) -> Result<Option<Minted>> {
    let Some(device) = hub_cli::saved_device(&connection.hub)? else {
        return Ok(None);
    };
    let client = forge_http::client().map_err(map_hub_error)?;
    let refused = |reply: &Value| {
        invalid(&refusal_text(
            reply["error"].as_str().unwrap_or_default(),
            &connection.repo,
            &connection.hub,
        ))
    };
    let status_url = format!("{}/v1/hub/device", connection.hub);
    let (status, me) = call(
        &client,
        Method::GET,
        &status_url,
        Some(&device.secret),
        None,
    )
    .await?;
    match status {
        200 => {}
        401 => {
            let _ = hub_cli::forget_device_if(&connection.hub, &device.secret);
            return Ok(None);
        }
        403 => return Err(refused(&me)),
        _ => return Err(invalid("Hub could not check this computer's sign-in")),
    }
    let owner_host = me["user"]["host"].as_str().and_then(identity_hostname);
    if owner_host.as_deref() != Some(connection.origin_host.as_str())
        || provider.is_some_and(|provider| me["user"]["provider"] != provider_name(provider))
    {
        return Err(invalid(
            "Hub sign-in identity does not match the checkout origin",
        ));
    }
    let body = json!({"name": token_name()?, "repository": connection.repo, "auto": auto});
    let url = format!("{}/v1/hub/device/tokens", connection.hub);
    let (status, reply) = call(
        &client,
        Method::POST,
        &url,
        Some(&device.secret),
        Some(body),
    )
    .await?;
    match status {
        201 => minted_from(&reply, HubAuth::Device(device.secret)).map(Some),
        401 => {
            let _ = hub_cli::forget_device_if(&connection.hub, &device.secret);
            Ok(None)
        }
        403 | 503 => Err(refused(&reply)),
        _ => Err(invalid("Hub could not create an agent token")),
    }
}

/// Gives this computer a credential from a live session, for sign-ins made before computers
/// had one. Best effort and silent.
async fn enroll(hub: &str) {
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
        .join(format!("{:x}.json", digest)))
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
    /// Try again later: no credential and no live session, the Hub was unreachable, or the
    /// connection changed meanwhile.
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
    if hub_cli::saved_device(&saved.hub)?.is_none() {
        enroll(&saved.hub).await;
    }
    let minted = match mint(&saved, None, false).await {
        Ok(Some(minted)) => minted,
        Ok(None) if hub_cli::is_signed_in(Some(&saved.hub)).unwrap_or(false) => {
            return Ok(Renewal::Unavailable);
        }
        Ok(None) => {
            return Ok(Renewal::Refused(refusal_text(
                "sign_in_required",
                &saved.repo,
                &saved.hub,
            )));
        }
        Err(Error::Invalid(message)) => return Ok(Renewal::Refused(message)),
        Err(_) => return Ok(Renewal::Unavailable),
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
    retire_owned(&saved).await;
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
    /// No Hub with auto-connect on has this repository.
    NotConnected,
    /// Auto-connect is off everywhere, or the checkout has no usable origin.
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
    let mut asked = false;
    for (hub, _) in hubs {
        let key = not_connected_key(&hub, &host, &repo);
        if crate::hook_cli::marked_within(&config, &key, NOT_CONNECTED_FOR) {
            asked = true;
            continue;
        }
        let args = ConnectArgs {
            hub: hub.clone(),
            no_sign_in: true,
            auto: true,
            ..ConnectArgs::default()
        };
        match connect_inner(args, root.to_owned(), None, None).await {
            Ok(()) => return AutoConnect::Connected(repo),
            Err(Error::Invalid(message))
                if message == refusal_text("not_connected", &repo, &hub) =>
            {
                crate::hook_cli::mark(&config, &key);
                asked = true;
            }
            Err(Error::Invalid(message)) if message == NO_CREDENTIAL => {}
            Err(Error::Invalid(message)) => return AutoConnect::Refused(message),
            Err(error) => return AutoConnect::Refused(error.to_string()),
        }
    }
    if asked {
        AutoConnect::NotConnected
    } else {
        AutoConnect::Skipped
    }
}

/// `agentscm client auto-connect`: prints one JSON line, `{"result": "connected" |
/// "refused" | "not_connected" | "skipped", "repository"?, "message"?}`.
pub(crate) async fn auto_connect_command(root: &Path) -> Result<()> {
    let result = auto_connect(root).await;
    match &result {
        AutoConnect::Connected(_) => record_attention(root, None),
        AutoConnect::Refused(message) => record_attention(root, Some(message)),
        AutoConnect::NotConnected | AutoConnect::Skipped => {}
    }
    let line = match result {
        AutoConnect::Connected(repo) => json!({"result": "connected", "repository": repo}),
        AutoConnect::Refused(message) => json!({"result": "refused", "message": message}),
        AutoConnect::NotConnected => json!({"result": "not_connected"}),
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
    fn every_refusal_names_the_repository_or_hub_and_a_fix() {
        for kind in [
            "not_connected",
            "no_repository_role",
            "access_check_unavailable",
            "permission_unavailable",
            "not_allowed",
            "user_disabled",
            "sign_in_required",
        ] {
            let text = refusal_text(kind, "acme/widgets", "https://hub.example");
            assert!(text.contains("https://hub.example"), "{kind}: {text}");
            assert!(text.contains(';'), "{kind} has no fix: {text}");
        }
    }
}
