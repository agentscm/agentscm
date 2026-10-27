//! Computers signed in to the Hub: plain data, validation and event constructors.
//! The store holds the state; `login` serves the routes.
use serde_json::json;

use super::store::{Event, User};
use super::{HubError, Result};

const MAX_DEVICE_NAME_CHARS: usize = 64;
/// A computer credential unused this long stops working.
pub const DEVICE_IDLE_SECONDS: i64 = 90 * 24 * 60 * 60;
/// `last_used_at` is written at most this often.
pub(super) const DEVICE_TOUCH_SECONDS: i64 = 60 * 60;
/// Agent tokens a computer mints live this long; the client renews them.
pub const DEVICE_AGENT_TOKEN_TTL_SECONDS: i64 = 24 * 60 * 60;
/// Read-only tokens a computer mints (for the local page) live this long.
pub const DEVICE_READ_TOKEN_TTL_SECONDS: i64 = 30 * 60;
/// Refused sign-ins kept for administrators.
pub(super) const MAX_REFUSALS: i64 = 200;
pub(super) const REFUSAL_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Computers listed per owner (or for an administrator).
pub(super) const MAX_DEVICES: i64 = 100;

/// 1 to 64 characters, no control characters, not blank.
pub fn validate_device_name(name: &str) -> Result<()> {
    if name.trim().is_empty()
        || name.chars().count() > MAX_DEVICE_NAME_CHARS
        || name.chars().any(char::is_control)
    {
        return Err(HubError::Invalid(
            "computer name must be 1 to 64 printable characters".into(),
        ));
    }
    Ok(())
}

/// A computer as its owner or an administrator sees it; never the secret or digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// `dev_` + 32 lowercase hex.
    pub id: String,
    pub owner: User,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked_at: Option<i64>,
    /// Repositories with a live agent token minted through this computer, sorted.
    pub repositories: Vec<String>,
}

/// An authenticated computer credential.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevicePrincipal {
    pub device_id: String,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub owner: User,
}

/// Why a sign-in or a computer mint was refused, for administrators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub provider: String,
    pub host: String,
    pub forge_user_id: i64,
    pub login: String,
    pub reason: String,
    /// `sign_in` or `device_mint`.
    pub via: String,
    pub at: i64,
    pub count: i64,
}

/// A connected repository with how many computers and live agent tokens use it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryUsage {
    pub full_name: String,
    pub host: String,
    pub connection: String,
    pub devices: i64,
    pub agent_tokens: i64,
}

impl Event {
    pub(super) fn device_created(device: &str, owner: &str, name: &str) -> Result<Self> {
        Self::new(
            "device_created",
            device,
            json!({"owner": owner, "name": name}),
        )
    }

    /// `by` is the acting user, or `dev_…` when a computer revoked another.
    pub(super) fn device_revoked(device: &str, by: &str) -> Result<Self> {
        Self::new("device_revoked", device, json!({"by": by}))
    }

    pub(super) fn device_renamed(device: &str, name: &str) -> Result<Self> {
        Self::new("device_renamed", device, json!({"name": name}))
    }

    pub(super) fn auto_connected(device: &str, token: &str, repository: &str) -> Result<Self> {
        Self::new(
            "auto_connected",
            device,
            json!({"token": token, "repository": repository}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_bounded_and_printable() {
        assert!(validate_device_name("brandonfla's MacBook Pro").is_ok());
        assert!(validate_device_name(&"é".repeat(64)).is_ok());
        for bad in ["", "   ", "a\nb", &"x".repeat(65)] {
            assert!(validate_device_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn device_digests_differ_from_agent_token_digests() {
        let raw = "a".repeat(64);
        use super::super::store::{agent_token_digest, device_digest};
        assert_ne!(
            device_digest(&raw).unwrap(),
            agent_token_digest(&raw).unwrap()
        );
    }
}
