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
    /// `pull_request`, `branch`, or a bounded `provider_context` snapshot.
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
    /// `attempting` (reserved, reply not recorded: uncertain), `posted`, or `refused` (the forge
    /// definitely rejected it; never success).
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

pub(super) fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(super) fn validate_check_phase(phase: &str) -> Result<()> {
    if matches!(phase, "posted" | "refused") {
        Ok(())
    } else {
        Err(invalid("forge check phase is not final"))
    }
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

/// One merge queue group the Hub saw, with the journal of the status it posted on it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeGroup {
    pub head_sha: String,
    /// The queue branch, without `refs/heads/`.
    pub head_ref: String,
    pub base_ref: String,
    pub base_sha: String,
    pub pr_number: i64,
    pub pr_head_sha: String,
    pub context: String,
    /// None while the group is current; once set it never clears.
    pub ended_reason: Option<String>,
    pub observed_at: i64,
    pub ended_at: Option<i64>,
    /// Compare-and-set counter for the status journal; 0 before the first reservation.
    pub generation: i64,
    /// The last reserved status, `None` before the first reservation.
    pub check: Option<ForgeCheck>,
    pub ruleset_requirement: Option<String>,
    pub status_source: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewMergeGroup {
    pub head_sha: String,
    pub head_ref: String,
    pub base_ref: String,
    pub base_sha: String,
    pub pr_number: i64,
    pub pr_head_sha: String,
    pub context: String,
}

impl NewMergeGroup {
    pub(super) fn validate(&self) -> Result<()> {
        validate_check_key(self.pr_number, &self.context)?;
        let name_ok = |name: &str| {
            !name.is_empty() && name.len() <= 255 && super::github::valid_ref_name(name)
        };
        if !is_lower_hex(&self.head_sha, 40)
            || !is_lower_hex(&self.base_sha, 40)
            || !is_lower_hex(&self.pr_head_sha, 40)
            || !name_ok(&self.head_ref)
            || !name_ok(&self.base_ref)
        {
            return Err(invalid("merge group is out of bounds"));
        }
        Ok(())
    }
}

/// A short static observation (an ended reason, a requirement or source label).
pub(super) fn validate_group_note(note: &str) -> Result<()> {
    if note.is_empty() || note.len() > 120 || note.chars().any(char::is_control) {
        return Err(invalid("merge group note is out of bounds"));
    }
    Ok(())
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
        "provider_context" => Ok(("provider_context_observed", "provider_context_removed")),
        _ => Err(invalid("unsupported observation kind")),
    }
}

/// The event kind for forgetting an observation of `kind`: a closed pull request, or a
/// branch that was deleted or merely stopped being tracked.
pub(super) fn gone_event(kind: &str, event: &str) -> Result<&'static str> {
    match (kind, event) {
        ("pull_request", "pull_request_closed") => Ok("pull_request_closed"),
        ("branch", "branch_deleted") => Ok("branch_deleted"),
        ("branch", "branch_unobserved") => Ok("branch_unobserved"),
        ("provider_context", "provider_context_removed") => Ok("provider_context_removed"),
        _ => Err(invalid("event does not fit the observation kind")),
    }
}

pub(super) fn validate_observation(kind: &str, key: &str, data: &str) -> Result<()> {
    observation_kinds(kind)?;
    if kind == "provider_context"
        && (data.len() > super::provider_context::MAX_SNAPSHOT_BYTES
            || (key != "_health" && !key.parse::<i64>().is_ok_and(|number| number > 0)))
    {
        return Err(invalid(
            "provider context exceeds its identity or size bounds",
        ));
    }
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

// The append-only log needs change identity, not another copy of retained evidence.
const MAX_PROVIDER_CONTEXT_EVENT_BYTES: usize = 1024;

fn event_availability(value: &Value) -> &'static str {
    match value.as_str() {
        Some("available") => "available",
        Some("incomplete") => "incomplete",
        Some("observed") => "observed",
        Some("permission_missing") => "permission_missing",
        Some("permissions_unavailable") => "permissions_unavailable",
        Some("permission_denied") => "permission_denied",
        Some("authentication_failed") => "authentication_failed",
        Some("rate_limited") => "rate_limited",
        Some("response_too_large") => "response_too_large",
        Some("identity_unavailable") => "identity_unavailable",
        Some("installation_suspended") => "installation_suspended",
        Some("timed_out") => "timed_out",
        Some("not_collected") => "not_collected",
        _ => "unavailable",
    }
}

fn event_head(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|head| {
            matches!(head.len(), 40 | 64) && head.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
        .map(str::to_ascii_lowercase)
}

fn provider_context_event(
    repository_id: i64,
    key: &str,
    data: &str,
    value: &Value,
    removed: bool,
) -> Result<Value> {
    // The storage key, rather than caller-controlled snapshot fields, identifies this row.
    let number = if key == "_health" {
        None
    } else {
        Some(
            key.parse::<i64>()
                .ok()
                .filter(|number| *number > 0)
                .ok_or_else(|| invalid("invalid provider context event key"))?,
        )
    };
    if removed {
        // Forget receives a tombstone, not the removed evidence's bytes. Record only
        // the removal identity; no digest or category state can identify that evidence.
        return Ok(json!({
            "summary_version": 1, "repository_id": repository_id,
            "scope": if number.is_some() { "pull_request" } else { "repository_health" },
            "number": number, "change": "removed", "head_sha": null,
            "categories": {}, "snapshot_digest": null
        }));
    }
    let mut categories = serde_json::Map::new();
    for category in ["reviews", "check_runs", "commit_statuses"] {
        let availability = event_availability(&value[category]["availability"]);
        categories.insert(category.into(), json!({
            "availability": availability,
            "complete": availability == "available" && value[category]["complete"].as_bool() == Some(true)
        }));
    }
    Ok(json!({
        "summary_version": 1, "repository_id": repository_id, "change": "observed",
        "scope": if number.is_some() { "pull_request" } else { "repository_health" },
        "number": number, "head_sha": event_head(&value["head_sha"]),
        "health_availability": number.is_none().then(|| event_availability(&value["availability"])),
        "permissions_availability": event_availability(&value["permissions"]["availability"]),
        "categories": categories,
        "snapshot_digest": {"algorithm": "sha256", "encoding": "hex",
            "covered_bytes": "observation_data_utf8", "value": super::store::lower_hex_of(data.as_bytes())}
    }))
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

    /// Legacy observations retain their data and repository display name. Provider-context
    /// observations retain a bounded summary and digest of the incoming data bytes. Their
    /// removals identify the deleted key, without claiming a digest of the removed evidence.
    /// Only the observation row holds the full data.
    pub(super) fn observation(
        kind: &'static str,
        repo: &ConnectedRepository,
        key: &str,
        data: &str,
    ) -> Result<Self> {
        let mut value: Value =
            serde_json::from_str(data).map_err(|error| HubError::Unavailable(error.to_string()))?;
        let provider_context = matches!(
            kind,
            "provider_context_observed" | "provider_context_removed"
        );
        if provider_context {
            value = provider_context_event(
                repo.repository_id,
                key,
                data,
                &value,
                kind == "provider_context_removed",
            )?;
        } else if let Some(object) = value.as_object_mut() {
            object.insert("repository_id".into(), json!(repo.repository_id));
            object.insert("full_name".into(), json!(repo.full_name));
        }
        let event = Self::new(kind, &repo.id, value)?;
        if provider_context && event.data.len() > MAX_PROVIDER_CONTEXT_EVENT_BYTES {
            return Err(HubError::Unavailable(
                "provider context event summary exceeds its byte limit".into(),
            ));
        }
        Ok(event)
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

    pub(super) fn notice(kind: &'static str, subject: &str, data: &str) -> Result<Self> {
        Ok(Self {
            id: super::store::new_id("evt")?,
            at: super::store::unix_now(),
            kind,
            subject: subject.to_owned(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn repository() -> ConnectedRepository {
        ConnectedRepository {
            id: "rep_00000000000000000000000000000001".into(),
            connection: "con_00000000000000000000000000000001".into(),
            host: "github.com".into(),
            repository_id: 4242,
            full_name: "acme/widgets".into(),
            node_id: None,
            installation_id: None,
            pulls_etag: None,
        }
    }

    #[test]
    fn provider_event_bounds_untrusted_fields_and_uses_storage_identity() {
        let data = json!({"repository_id": 999, "number": 999,
            "head_sha": "a".repeat(2000), "availability": "x".repeat(2000),
            "permissions": {"availability": "x".repeat(2000)},
            "reviews": {"availability": "x".repeat(2000), "complete": true, "items": ["not-in-event"]},
            "check_runs": {"availability": "available", "complete": true},
            "commit_statuses": {"availability": "timed_out", "complete": true}}).to_string();
        validate_observation("provider_context", "17", &data).unwrap();
        let event =
            Event::observation("provider_context_observed", &repository(), "17", &data).unwrap();
        assert!(event.data.len() <= MAX_PROVIDER_CONTEXT_EVENT_BYTES);
        let value: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(value["repository_id"], 4242);
        assert_eq!(value["number"], 17);
        assert_eq!(value["scope"], "pull_request");
        assert!(value["head_sha"].is_null());
        assert!(value["health_availability"].is_null());
        assert_eq!(value["permissions_availability"], "unavailable");
        assert_eq!(
            value["categories"]["reviews"]["availability"],
            "unavailable"
        );
        assert_eq!(value["categories"]["reviews"]["complete"], false);
        assert_eq!(value["categories"]["check_runs"]["complete"], true);
        assert_eq!(value["categories"]["commit_statuses"]["complete"], false);
        assert!(!event.data.contains("not-in-event"));
    }

    #[test]
    fn removed_provider_event_does_not_claim_tombstone_evidence() {
        let data = json!({"number": 999, "availability": "closed_or_gone",
            "head_sha": "a".repeat(40), "permissions": {"availability": "observed"},
            "reviews": {"availability": "available", "complete": true}})
        .to_string();
        let event =
            Event::observation("provider_context_removed", &repository(), "17", &data).unwrap();
        let value: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(value["number"], 17);
        assert_eq!(value["repository_id"], 4242);
        assert_eq!(value["scope"], "pull_request");
        assert_eq!(value["change"], "removed");
        assert_eq!(value.get("head_sha"), Some(&Value::Null));
        assert_eq!(value.get("snapshot_digest"), Some(&Value::Null));
        assert_eq!(value["categories"], json!({}));
        assert!(value.get("permissions_availability").is_none());
        assert!(event.data.len() <= MAX_PROVIDER_CONTEXT_EVENT_BYTES);
    }

    #[test]
    fn provider_event_preserves_real_category_availability_and_completeness() {
        // Same category shape as provider_context::category: successful bounded reads
        // use available/incomplete; observed describes permission discovery instead.
        let data = json!({"version": 1, "repository_id": 4242, "number": 17,
            "head_sha": "a".repeat(40), "permissions": {"availability": "observed"},
            "reviews": {"view": "latest_observed", "availability": "available", "complete": true,
                "items": [], "omitted": 0, "malformed": 0, "pages_read": 1, "unread_pages": 0},
            "check_runs": {"view": "latest_by_name", "availability": "incomplete", "complete": false,
                "items": [], "omitted": 0, "malformed": 1, "pages_read": 1, "unread_pages": 0},
            "commit_statuses": {"availability": "permission_missing", "complete": false,
                "items": [], "omitted": 0, "malformed": 0, "pages_read": 0, "unread_pages": null}
        }).to_string();
        let event =
            Event::observation("provider_context_observed", &repository(), "17", &data).unwrap();
        let value: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(value["permissions_availability"], "observed");
        assert_eq!(
            value["categories"]["reviews"],
            json!({"availability": "available", "complete": true})
        );
        assert_eq!(
            value["categories"]["check_runs"],
            json!({"availability": "incomplete", "complete": false})
        );
        assert_eq!(
            value["categories"]["commit_statuses"],
            json!({"availability": "permission_missing", "complete": false})
        );
        assert!(event.data.len() <= MAX_PROVIDER_CONTEXT_EVENT_BYTES);
    }

    #[test]
    fn health_summary_identity_is_not_taken_from_snapshot_number() {
        let data = json!({"number": 17, "availability": "available", "permissions": {"availability": "observed"}}).to_string();
        let event =
            Event::observation("provider_context_observed", &repository(), "_health", &data)
                .unwrap();
        let value: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(value["scope"], "repository_health");
        assert!(value["number"].is_null());
        assert_eq!(value["health_availability"], "available");
        assert!(event.data.len() <= MAX_PROVIDER_CONTEXT_EVENT_BYTES);
    }

    #[test]
    fn legacy_pull_and_branch_event_payloads_are_unchanged() {
        for (kind, key, original) in [
            (
                "pull_request_observed",
                "17",
                json!({"number": 17, "head_sha": "a".repeat(40), "files": ["src/lib.rs"]}),
            ),
            (
                "branch_observed",
                "feature/widgets",
                json!({"name": "feature/widgets", "head_sha": "b".repeat(40)}),
            ),
        ] {
            let event =
                Event::observation(kind, &repository(), key, &original.to_string()).unwrap();
            let mut expected = original;
            expected["repository_id"] = json!(4242);
            expected["full_name"] = json!("acme/widgets");
            assert_eq!(
                serde_json::from_str::<Value>(&event.data).unwrap(),
                expected
            );
        }
    }
}
