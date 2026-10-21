//! Forge connections, the repositories they reach, and what the forge showed
//! about them. Plain data and event constructors; the store holds the state.
use serde_json::{Value, json};

use super::store::{Event, ForgeIdentity};
use super::{HubError, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewConnection {
    /// `con_` + 32 lowercase hex, chosen by the caller (a manifest flow puts it in the webhook URL).
    pub id: String,
    pub provider: String,
    pub host: String,
    /// `app` or `pat`.
    pub kind: String,
    pub app_id: Option<i64>,
    pub app_slug: Option<String>,
    /// The login a `pat` connection acts as.
    pub acts_as: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Connection {
    pub id: String,
    pub provider: String,
    pub host: String,
    pub kind: String,
    pub app_id: Option<i64>,
    pub app_slug: Option<String>,
    pub acts_as: Option<String>,
    pub outbound: bool,
    /// `enabled` or `auth_failed`.
    pub status: String,
    pub status_detail: Option<String>,
    pub next_poll_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectedRepository {
    /// `rep_` + 32 lowercase hex.
    pub id: String,
    pub connection: String,
    pub host: String,
    pub repository_id: i64,
    pub full_name: String,
    pub node_id: Option<String>,
    pub installation_id: Option<i64>,
    pub pulls_etag: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Observed {
    /// A newer read is already stored.
    Stale,
    Unchanged,
    Changed {
        previous: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    /// `pull_request` or `branch`.
    pub kind: String,
    /// PR number in decimal, or the branch name.
    pub key: String,
    /// Canonical JSON.
    pub data: String,
    /// Unix milliseconds when the forge read started.
    pub observed_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outbound {
    pub comment_id: Option<i64>,
    pub body_sha256: Option<String>,
    pub status_sha: Option<String>,
    pub status_description: Option<String>,
}

/// One Hub-posted commit status per (repository, pull request, context).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForgeCheck {
    pub head_sha: String,
    pub state: String,
    pub binding_digest: Option<String>,
    pub description: String,
    /// `attempting` (reserved, reply not recorded: uncertain) or `posted`.
    pub phase: String,
    pub generation: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewForgeCheck {
    pub head_sha: String,
    pub state: String,
    pub binding_digest: Option<String>,
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForgeCheckReserve {
    Reserved(i64),
    Conflict,
}

const MAX_CHECK_CONTEXT_BYTES: usize = 120;
const MAX_CHECK_DESCRIPTION_CHARS: usize = 140;

fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) fn validate_check_key(number: i64, context: &str) -> Result<()> {
    if number < 1
        || context.is_empty()
        || context.len() > MAX_CHECK_CONTEXT_BYTES
        || !context
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'_' | b'.' | b'-'))
    {
        return Err(invalid("forge check key is out of bounds"));
    }
    Ok(())
}

impl NewForgeCheck {
    pub(super) fn validate(&self) -> Result<()> {
        let digest_ok = match (self.state.as_str(), &self.binding_digest) {
            ("success", Some(digest)) => is_lower_hex(digest, 64),
            ("pending" | "failure" | "error", None) => true,
            _ => false,
        };
        if !is_lower_hex(&self.head_sha, 40)
            || !digest_ok
            || self.description.chars().count() > MAX_CHECK_DESCRIPTION_CHARS
            || self.description.chars().any(char::is_control)
        {
            return Err(invalid("forge check is out of bounds"));
        }
        Ok(())
    }
}

impl NewConnection {
    pub(super) fn validate(&self) -> Result<()> {
        let id_ok = self.id.strip_prefix("con_").is_some_and(|hex| {
            hex.len() == 32 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        });
        if !id_ok {
            return Err(invalid("connection id must be con_ and 32 lowercase hex"));
        }
        if !matches!(self.provider.as_str(), "github" | "gitlab")
            || (self.provider == "gitlab" && self.kind != "pat")
        {
            return Err(invalid(
                "connection provider must be github, or gitlab with kind pat",
            ));
        }
        // Reuses the host rules of a forge identity.
        ForgeIdentity {
            provider: self.provider.clone(),
            host: self.host.clone(),
            user_id: 1,
        }
        .validate()?;
        let shape_ok = match self.kind.as_str() {
            "app" => {
                self.app_id.is_some_and(|id| id > 0)
                    && self.app_slug.as_deref().is_some_and(plain)
                    && self.acts_as.is_none()
            }
            "pat" => {
                self.app_id.is_none()
                    && self.app_slug.is_none()
                    && self.acts_as.as_deref().is_some_and(plain)
            }
            _ => false,
        };
        if !shape_ok {
            return Err(invalid(
                "connection kind app needs app_id and app_slug; pat needs acts_as",
            ));
        }
        Ok(())
    }
}

fn plain(text: &str) -> bool {
    !text.is_empty() && text.len() <= 255 && !text.chars().any(char::is_control)
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

/// `(observed, gone)` event kinds of an observation kind.
pub(super) fn observation_kinds(kind: &str) -> Result<(&'static str, &'static str)> {
    match kind {
        "pull_request" => Ok(("pull_request_observed", "pull_request_closed")),
        "branch" => Ok(("branch_observed", "branch_deleted")),
        _ => Err(invalid("observation kind must be pull_request or branch")),
    }
}

/// The event kind for forgetting an observation of `kind`: a closed pull request, or a
/// branch that was deleted or merely stopped being tracked.
pub(super) fn gone_event(kind: &str, event: &str) -> Result<&'static str> {
    match (kind, event) {
        ("pull_request", "pull_request_closed") => Ok("pull_request_closed"),
        ("branch", "branch_deleted") => Ok("branch_deleted"),
        ("branch", "branch_unobserved") => Ok("branch_unobserved"),
        _ => Err(invalid("event does not fit the observation kind")),
    }
}

pub(super) fn validate_observation(kind: &str, key: &str, data: &str) -> Result<()> {
    observation_kinds(kind)?;
    if key.is_empty() || key.len() > 1024 || key.chars().any(char::is_control) {
        return Err(invalid(
            "observation key must be 1-1024 bytes without control characters",
        ));
    }
    if !serde_json::from_str::<Value>(data).is_ok_and(|value| value.is_object()) {
        return Err(invalid("observation data must be a JSON object"));
    }
    Ok(())
}

impl Event {
    pub(super) fn connection_created(new: &NewConnection, by: &str) -> Result<Self> {
        let data = json!({"provider": new.provider, "host": new.host, "kind": new.kind,
            "app_id": new.app_id, "app_slug": new.app_slug, "acts_as": new.acts_as, "by": by});
        Self::new("connection_created", &new.id, data)
    }

    pub(super) fn connection_status(id: &str, status: &str, detail: Option<&str>) -> Result<Self> {
        Self::new(
            "connection_status",
            id,
            json!({"status": status, "detail": detail}),
        )
    }

    pub(super) fn connection_outbound(id: &str, enabled: bool, by: &str) -> Result<Self> {
        Self::new(
            "connection_outbound",
            id,
            json!({"enabled": enabled, "by": by}),
        )
    }

    pub(super) fn repository_connected(repo: &ConnectedRepository) -> Result<Self> {
        let data = json!({"connection": repo.connection, "repository_id": repo.repository_id,
            "full_name": repo.full_name, "node_id": repo.node_id,
            "installation_id": repo.installation_id});
        Self::new("repository_connected", &repo.id, data)
    }

    pub(super) fn repository_renamed(repo: &ConnectedRepository, previous: &str) -> Result<Self> {
        let data = json!({"repository_id": repo.repository_id, "previous": previous,
            "full_name": repo.full_name});
        Self::new("repository_renamed", &repo.id, data)
    }

    /// The observation's data with the repository's ids added, so an event stands alone.
    pub(super) fn observation(
        kind: &'static str,
        repo: &ConnectedRepository,
        data: &str,
    ) -> Result<Self> {
        let mut value: Value =
            serde_json::from_str(data).map_err(|error| HubError::Unavailable(error.to_string()))?;
        if let Some(object) = value.as_object_mut() {
            object.insert("repository_id".into(), json!(repo.repository_id));
            object.insert("full_name".into(), json!(repo.full_name));
        }
        Self::new(kind, &repo.id, value)
    }

    pub(super) fn pr_comment_written(
        repo: &ConnectedRepository,
        number: i64,
        comment_id: i64,
    ) -> Result<Self> {
        let data = json!({"repository_id": repo.repository_id, "number": number,
            "comment_id": comment_id});
        Self::new("pr_comment_written", &repo.id, data)
    }

    pub(super) fn pr_status_written(
        repo: &ConnectedRepository,
        number: i64,
        sha: &str,
        description: &str,
    ) -> Result<Self> {
        let data = json!({"repository_id": repo.repository_id, "number": number,
            "sha": sha, "description": description});
        Self::new("pr_status_written", &repo.id, data)
    }

    pub(super) fn pr_notice(intent: &str, data: &str) -> Result<Self> {
        Ok(Self {
            id: super::store::new_id("evt")?,
            at: super::store::unix_now(),
            kind: "pr_notice",
            subject: intent.to_owned(),
            data: data.to_owned(),
        })
    }
}

/// Delivery ids are remembered this long; GitHub redelivers within hours, not weeks.
pub(super) const DELIVERY_KEEP_SECONDS: i64 = 7 * 24 * 60 * 60;

/// Column order read by both backends' row mappers.
pub(super) const CONNECTION_SELECT: &str = "SELECT id, provider, host, kind, app_id, app_slug,
    acts_as, outbound, status, status_detail, next_poll_at FROM hub_connections";

pub(super) const REPOSITORY_SELECT: &str = "SELECT id, connection, host, repository_id, full_name,
    node_id, installation_id, pulls_etag FROM hub_repositories";

pub(super) fn validate_status(status: &str, detail: Option<&str>) -> Result<()> {
    if !matches!(status, "enabled" | "auth_failed") || detail.is_some_and(|d| !plain(d)) {
        return Err(invalid(
            "status must be enabled or auth_failed, with a detail of 1-255 bytes",
        ));
    }
    Ok(())
}

pub(super) fn validate_repository_row(repository_id: i64, full_name: &str) -> Result<()> {
    if repository_id <= 0 {
        return Err(invalid("repository id must be positive"));
    }
    super::store::validate_repository(full_name)
}
