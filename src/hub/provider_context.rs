//! Bounded, read-only provider observations. These snapshots describe one non-atomic read;
//! they do not verify checks, infer attempts or grant merge authority.
use serde_json::{Value, json};

use super::connection::{ConnectedRepository, Connection};
use super::github::{Gh, GhError, Paged, parse_rfc3339};
use super::{HubState, Result};

pub const MAX_SNAPSHOT_BYTES: usize = 16 * 1024;
const MAX_ITEMS: usize = 8;
const CATEGORIES: [&str; 3] = ["reviews", "check_runs", "commit_statuses"];

fn sha(value: &Value) -> Option<&str> {
    value
        .as_str()
        .filter(|value| value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn text(value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    if raw.is_empty() || raw.chars().any(char::is_control) {
        return None;
    }
    Some(raw.chars().take(120).collect())
}

fn source_url(value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    if raw.len() > 512 || raw.chars().any(char::is_control) {
        return None;
    }
    let url = reqwest::Url::parse(raw).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none_or(|fragment| {
            fragment.len() <= 120
                && fragment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        }))
    .then(|| raw.to_owned())
}

fn timestamp(value: &Value) -> Option<String> {
    let raw = value.as_str()?;
    (raw.len() <= 40 && parse_rfc3339(raw).is_some()).then(|| raw.to_owned())
}

fn unavailable(reason: &str) -> Value {
    json!({"availability": reason, "complete": false, "items": [],
        "omitted": 0, "malformed": 0, "pages_read": 0, "unread_pages": null})
}

pub(super) fn error_availability(error: &GhError) -> &'static str {
    match error {
        GhError::Auth(detail) if detail.starts_with("403") => "permission_denied",
        GhError::Auth(_) => "authentication_failed",
        GhError::Limited(_) => "rate_limited",
        // Ordinary PR rereads can carry a permission refusal as Failed when
        // GitHub includes non-exhausted rate headers. It is still a known 403.
        GhError::Failed(detail) if detail.starts_with("403 ") => "permission_denied",
        GhError::Failed(_) => "unavailable",
        GhError::TooLarge(_) => "response_too_large",
    }
}

fn readable(permissions: &Value, key: &str) -> bool {
    matches!(permissions[key].as_str(), Some("read" | "write"))
}

/// Missing or malformed grants never count as readable. Write includes the corresponding
/// read capability; this does not request or upgrade an installation permission.
fn permission_state(permissions: Option<&Value>, key: &str) -> &'static str {
    match permissions {
        Some(permissions) if readable(permissions, key) => "available",
        Some(_) => "permission_missing",
        None => "permissions_unavailable",
    }
}

fn installation_permissions(
    value: &Value,
    installation: i64,
) -> std::result::Result<&Value, &'static str> {
    if installation <= 0 || value["id"].as_i64() != Some(installation) {
        return Err("identity_unavailable");
    }
    if !value["suspended_at"].is_null() {
        return Err("installation_suspended");
    }
    value
        .get("permissions")
        .filter(|value| value.is_object())
        .ok_or("permissions_unavailable")
}

fn item(kind: &str, row: &Value, head: &str) -> Option<Value> {
    let id = row["id"].as_i64().filter(|id| *id > 0)?;
    let mut output = json!({"provider_id": id, "attempt": null, "tested_merge_sha": null});
    let commit = match kind {
        "reviews" => {
            let state = row["state"].as_str()?;
            if !matches!(
                state,
                "APPROVED" | "CHANGES_REQUESTED" | "COMMENTED" | "DISMISSED" | "PENDING"
            ) {
                return None;
            }
            output["state"] = json!(state);
            output["reviewer"] = json!(text(&row["user"]["login"]));
            output["provider_submitted_at"] = json!(timestamp(&row["submitted_at"])?);
            output["source_url"] = json!(source_url(&row["html_url"])?);
            sha(&row["commit_id"])?
        }
        "check_runs" => {
            let commit = sha(&row["head_sha"])?;
            // A row for a different commit cannot describe this commit-scoped endpoint.
            if !commit.eq_ignore_ascii_case(head) {
                return None;
            }
            let status = row["status"].as_str()?;
            if !matches!(
                status,
                "queued" | "in_progress" | "completed" | "waiting" | "requested" | "pending"
            ) {
                return None;
            }
            let conclusion = row["conclusion"].as_str();
            if !row["conclusion"].is_null() && conclusion.is_none() {
                return None;
            }
            if let Some(conclusion) = conclusion
                && !matches!(
                    conclusion,
                    "success"
                        | "failure"
                        | "neutral"
                        | "cancelled"
                        | "skipped"
                        | "timed_out"
                        | "action_required"
                        | "stale"
                        | "startup_failure"
                )
            {
                return None;
            }
            if status == "completed" && conclusion.is_none() {
                return None;
            }
            output["name"] = json!(text(&row["name"])?);
            output["state"] = json!(status);
            output["conclusion"] = json!(conclusion);
            output["source_url"] = json!(source_url(&row["html_url"])?);
            // Queued check runs may not have started yet.
            for field in ["started_at", "completed_at"] {
                if !row[field].is_null() && timestamp(&row[field]).is_none() {
                    return None;
                }
            }
            output["provider_started_at"] = json!(timestamp(&row["started_at"]));
            output["provider_completed_at"] = json!(timestamp(&row["completed_at"]));
            if status == "completed" && output["provider_completed_at"].is_null() {
                return None;
            }
            commit
        }
        "commit_statuses" => {
            let state = row["state"].as_str()?;
            if !matches!(state, "pending" | "success" | "failure" | "error") {
                return None;
            }
            if let Some(commit) = row.get("sha")
                && !sha(commit).is_some_and(|commit| commit.eq_ignore_ascii_case(head))
            {
                return None;
            }
            output["name"] = json!(text(&row["context"])?);
            output["state"] = json!(state);
            output["provider_updated_at"] = json!(timestamp(&row["updated_at"])?);
            output["source_url"] = json!(source_url(&row["url"])?);
            // GitHub statuses have no SHA field: identity comes from the exact requested URL.
            output["commit_binding"] = json!("requested_commit_endpoint");
            head
        }
        _ => return None,
    };
    output["commit_sha"] = json!(commit.to_ascii_lowercase());
    output["head_relation"] = json!(if commit.eq_ignore_ascii_case(head) {
        "same"
    } else {
        "different"
    });
    Some(output)
}

fn category(kind: &str, result: Gh<Paged>, head: &str) -> Value {
    let page = match result {
        Ok(page) => page,
        Err(error) => return unavailable(error_availability(&error)),
    };
    let mut candidates = Vec::new();
    let mut malformed = 0;
    let mut ids = std::collections::BTreeSet::new();
    for row in &page.items {
        match item(kind, row, head) {
            Some(value) if ids.insert(value["provider_id"].as_i64().unwrap_or_default()) => {
                candidates.push(value);
            }
            _ => malformed += 1,
        }
    }
    if kind == "reviews" {
        candidates.sort_by(|left, right| {
            let order = |value: &Value| {
                (
                    value["provider_submitted_at"]
                        .as_str()
                        .and_then(parse_rfc3339)
                        .unwrap_or_default(),
                    value["provider_id"].as_i64().unwrap_or_default(),
                )
            };
            order(right).cmp(&order(left))
        });
    }
    let omitted = candidates.len().saturating_sub(MAX_ITEMS);
    candidates.truncate(MAX_ITEMS);
    let items = candidates;
    let complete = page.unread == 0 && malformed == 0 && omitted == 0;
    json!({"view": if kind == "check_runs" { "latest_by_name" } else if kind == "reviews" { "latest_observed" } else { "provider_list" }, "availability": if complete { "available" } else { "incomplete" },
        "complete": complete, "items": items, "omitted": omitted,
        "malformed": malformed, "pages_read": page.pages, "unread_pages": page.unread})
}

fn bound(snapshot: &mut Value) {
    while serde_json::to_vec(snapshot).map_or(true, |bytes| bytes.len() > MAX_SNAPSHOT_BYTES) {
        let largest = CATEGORIES
            .iter()
            .filter_map(|kind| {
                let items = snapshot[kind]["items"].as_array()?;
                (!items.is_empty()).then_some((*kind, items.len()))
            })
            .max_by_key(|(_, len)| *len);
        let Some((kind, _)) = largest else { break };
        snapshot[kind]["items"]
            .as_array_mut()
            .expect("category array")
            .pop();
        let omitted = snapshot[kind]["omitted"].as_u64().unwrap_or(0);
        snapshot[kind]["omitted"] = json!(omitted + 1);
        snapshot[kind]["availability"] = json!("incomplete");
        snapshot[kind]["complete"] = json!(false);
        snapshot["retention_truncated"] = json!(true);
    }
}

/// Clears retained provider data after a bounded sync times out or becomes unavailable.
pub fn unavailable_snapshot(
    repo: &ConnectedRepository,
    pr: &Value,
    observed_at: i64,
    reason: &str,
) -> Value {
    let reason = match reason {
        "timed_out"
        | "not_collected"
        | "identity_unavailable"
        | "authentication_failed"
        | "permission_denied"
        | "rate_limited" => reason,
        _ => "unavailable",
    };
    let mut snapshot = json!({"version": 1, "provider": "github",
        "repository_id": repo.repository_id, "number": pr["number"].as_i64().filter(|number| *number > 0),
        "head_sha": sha(&pr["head_sha"]), "base_sha": sha(&pr["base_sha"]), "observed_at": observed_at,
        "atomic_snapshot": false, "attempt_history": false,
        "retention_truncated": false, "item_limit": MAX_ITEMS, "page_limit": 2,
        "page_limits": {"reviews": 2, "check_runs": 1, "commit_statuses": 1},
        "source_url": source_url(&pr["html_url"]),
        "permissions": {"availability": "unavailable", "inspection": "not_observed", "requester_access": "not_asserted"}});
    for kind in CATEGORIES {
        snapshot[kind] = unavailable(reason);
    }
    snapshot
}

/// Collects ordinary metadata from normalized retained PR identity under the existing
/// serialized repository sync. Each snapshot replaces the current observation; it does not
/// retain attempt history, assert atomic consistency or request additional access.
pub async fn collect(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    pr: &Value,
    observed_at: i64,
) -> Result<Value> {
    let head = sha(&pr["head_sha"]);
    let base = sha(&pr["base_sha"]);
    let number = pr["number"].as_i64().filter(|number| *number > 0);
    let identity_ok = pr["repository_id"].as_i64() == Some(repo.repository_id)
        && repo.repository_id > 0
        && number.is_some()
        && head.is_some()
        && base.is_some();
    let mut snapshot = unavailable_snapshot(repo, pr, observed_at, "identity_unavailable");
    if !identity_ok || observed_at < 0 || conn.provider != "github" {
        return Ok(snapshot);
    }
    let api = match state.sync.api() {
        Ok(api) => api,
        Err(_) => {
            for kind in CATEGORIES {
                snapshot[kind] = unavailable("unavailable");
            }
            return Ok(snapshot);
        }
    };
    let mut grant_failure = "permissions_unavailable";
    let grants = if conn.kind == "app" {
        match (repo.installation_id, state.sync.jwt(conn).await) {
            (Some(installation), Ok(jwt)) => match api.installation(&jwt, installation).await {
                Ok(Some(value)) => match installation_permissions(&value, installation) {
                    Ok(_) => Some(value),
                    Err(reason) => {
                        grant_failure = reason;
                        None
                    }
                },
                Err(error) => {
                    grant_failure = error_availability(&error);
                    None
                }
                _ => None,
            },
            (_, Err(error)) => {
                grant_failure = error_availability(&error);
                None
            }
            _ => None,
        }
    } else {
        None
    };
    let permissions = grants
        .as_ref()
        .and_then(|value| value.get("permissions"))
        .filter(|value| value.is_object());
    snapshot["permissions"] = json!({
        "availability": if permissions.is_some() { "observed" } else { grant_failure },
        "inspection": if permissions.is_some() { "confirmed" } else if conn.kind == "app" { "failed" } else { "not_attempted" },
        "pull_requests": permission_state(permissions, "pull_requests"),
        "checks": permission_state(permissions, "checks"),
        "statuses": permission_state(permissions, "statuses"),
        "requester_access": "not_asserted"});
    let head = head.expect("validated head");
    let mut blocked = None;
    for (kind, grant) in [
        ("reviews", "pull_requests"),
        ("check_runs", "checks"),
        ("commit_statuses", "statuses"),
    ] {
        let availability = permission_state(permissions, grant);
        snapshot[kind] = if permissions.is_none() {
            unavailable(grant_failure)
        } else if let Some(reason) = blocked {
            unavailable(reason)
        } else if availability != "available" {
            unavailable(availability)
        } else {
            let result = match kind {
                "reviews" => {
                    api.context_reviews(token, &repo.full_name, number.expect("validated number"))
                        .await
                }
                "check_runs" => api.context_checks(token, &repo.full_name, head).await,
                _ => api.context_statuses(token, &repo.full_name, head).await,
            };
            blocked = match &result {
                Err(GhError::Limited(_)) => Some("rate_limited"),
                Err(GhError::Auth(detail)) if detail.starts_with("403") => {
                    Some("permission_denied")
                }
                Err(GhError::Auth(_)) => Some("authentication_failed"),
                _ => None,
            };
            category(kind, result, head)
        };
    }
    bound(&mut snapshot);
    Ok(snapshot)
}

/// Only project a retained snapshot for the exact currently observed repository, PR, head
/// and base. Freshness and collection coverage remain separate display facts.
pub fn project(
    snapshot: &Value,
    repository_id: i64,
    number: i64,
    head: &str,
    base: &str,
) -> Option<Value> {
    (snapshot["version"] == 1
        && snapshot["provider"] == "github"
        && repository_id > 0
        && number > 0
        && snapshot["repository_id"].as_i64() == Some(repository_id)
        && snapshot["number"].as_i64() == Some(number)
        && sha(&snapshot["head_sha"]).is_some_and(|sha| sha.eq_ignore_ascii_case(head))
        && sha(&snapshot["base_sha"]).is_some_and(|sha| sha.eq_ignore_ascii_case(base))
        && snapshot["observed_at"]
            .as_i64()
            .is_some_and(|stamp| stamp >= 0)
        && serde_json::to_vec(snapshot).is_ok_and(|bytes| bytes.len() <= MAX_SNAPSHOT_BYTES))
    .then(|| snapshot.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const BASE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn paged(items: Vec<Value>, unread: u32) -> Gh<Paged> {
        Ok(Paged {
            items,
            etag: None,
            pages: 1,
            unread,
        })
    }

    fn status(id: i64) -> Value {
        json!({"id": id, "state": "success", "context": "test", "updated_at": "2026-10-30T10:00:00Z", "url": "https://api.github.com/repos/example/project/statuses/1"})
    }

    #[test]
    fn malformed_rows_and_partial_pages_never_become_complete() {
        for (rows, unread) in [
            (vec![json!({})], 0),
            (vec![status(1)], 1),
            (vec![status(1), status(1)], 0),
        ] {
            let category = category("commit_statuses", paged(rows, unread), HEAD);
            assert_eq!(category["complete"], false);
            assert_eq!(category["availability"], "incomplete");
        }
    }

    #[test]
    fn mismatched_check_commit_is_not_retained() {
        let row =
            json!({"id": 1, "head_sha": BASE, "status": "completed", "conclusion": "success"});
        let category = category("check_runs", paged(vec![row], 0), HEAD);
        assert_eq!(category["items"], json!([]));
        assert_eq!(category["malformed"], 1);
        assert_eq!(category["complete"], false);
    }

    #[test]
    fn missing_denied_and_write_grants_remain_distinct() {
        assert_eq!(permission_state(None, "checks"), "permissions_unavailable");
        for permissions in [
            json!({}),
            json!({"checks": "none"}),
            json!({"checks": true}),
        ] {
            assert_eq!(
                permission_state(Some(&permissions), "checks"),
                "permission_missing"
            );
        }
        assert_eq!(
            permission_state(Some(&json!({"pull_requests":"write"})), "pull_requests"),
            "available"
        );
        assert_eq!(
            permission_state(Some(&json!({"pull_requests":"write"})), "checks"),
            "permission_missing"
        );
        assert_eq!(
            category(
                "check_runs",
                Err(GhError::Auth("private detail".into())),
                HEAD
            )["availability"],
            "authentication_failed"
        );
        assert_eq!(
            category("check_runs", Err(GhError::Limited(123)), HEAD)["availability"],
            "rate_limited"
        );
        let denied = category("reviews", Err(GhError::Auth("403 from repos".into())), HEAD);
        assert_eq!(denied["availability"], "permission_denied");
        assert_eq!(denied["items"], json!([]));
        assert_eq!(denied["complete"], false);
        assert_eq!(
            error_availability(&GhError::Failed("403 from repos".into())),
            "permission_denied"
        );
    }

    #[test]
    fn reviews_choose_newest_observed_rows_and_preserve_prior_page_gaps() {
        let rows = (1..=10).map(|id| json!({
            "id": id, "state": "APPROVED", "commit_id": HEAD,
            "submitted_at": format!("2026-10-30T10:00:{id:02}Z"),
            "html_url": format!("https://github.com/example/project/pull/3#pullrequestreview-{id}")
        })).collect();
        let result = category("reviews", paged(rows, 1), HEAD);
        assert_eq!(result["view"], "latest_observed");
        assert_eq!(result["items"][0]["provider_id"], 10);
        assert_eq!(result["items"][7]["provider_id"], 3);
        assert_eq!(result["omitted"], 2);
        assert_eq!(result["unread_pages"], 1);
        assert_eq!(result["complete"], false);
        // Provider IDs settle ties, but cannot override a later submission time.
        let rows = vec![
            json!({"id": 100, "state": "APPROVED", "commit_id": HEAD, "submitted_at": "2026-10-30T10:00:01Z", "html_url": "https://github.com/example/project/pull/3#pullrequestreview-100"}),
            json!({"id": 2, "state": "CHANGES_REQUESTED", "commit_id": HEAD, "submitted_at": "2026-10-30T10:00:02Z", "html_url": "https://github.com/example/project/pull/3#pullrequestreview-2"}),
            json!({"id": 3, "state": "COMMENTED", "commit_id": HEAD, "submitted_at": "2026-10-30T10:00:02Z", "html_url": "https://github.com/example/project/pull/3#pullrequestreview-3"}),
        ];
        let result = category("reviews", paged(rows, 0), HEAD);
        assert_eq!(result["items"][0]["provider_id"], 3);
        assert_eq!(result["items"][1]["provider_id"], 2);
        assert_eq!(result["items"][2]["provider_id"], 100);
    }

    #[test]
    fn installation_identity_and_suspension_fence_grants() {
        let grants = json!({"id": 4, "permissions": {"checks": "read"}, "suspended_at": null});
        assert!(installation_permissions(&grants, 4).is_ok());
        assert_eq!(
            installation_permissions(&grants, 5),
            Err("identity_unavailable")
        );
        let suspended = json!({"id": 4, "permissions": {"checks": "read"}, "suspended_at": "2026-10-30T10:00:00Z"});
        assert_eq!(
            installation_permissions(&suspended, 4),
            Err("installation_suspended")
        );
    }

    #[test]
    fn review_anchor_and_old_commit_are_retained_without_current_approval_claim() {
        let review = json!({"id": 1, "state": "APPROVED", "commit_id": BASE,
            "submitted_at": "2026-10-30T10:00:00Z", "user": {"login": "reviewer-fixture"},
            "html_url": "https://github.com/example/project/pull/3#pullrequestreview-123"});
        let projected = category("reviews", paged(vec![review.clone()], 0), HEAD);
        assert_eq!(projected["complete"], true);
        assert_eq!(projected["items"][0]["head_relation"], "different");
        assert_eq!(projected["items"][0]["commit_sha"], BASE);
        assert_eq!(
            projected["items"][0]["provider_submitted_at"],
            "2026-10-30T10:00:00Z"
        );
        assert!(projected["items"][0].get("provider_updated_at").is_none());
        assert_eq!(projected["items"][0]["reviewer"], "reviewer-fixture");
        let mut unknown = review;
        unknown.as_object_mut().unwrap().remove("user");
        let projected = category("reviews", paged(vec![unknown], 0), HEAD);
        assert!(projected["items"][0]["reviewer"].is_null());
    }

    #[test]
    fn retained_items_are_bounded_and_do_not_copy_bodies_or_infer_attempts() {
        let mut row = status(1);
        row["body"] = json!("private body");
        row["context"] = json!("x".repeat(10000));
        let category = category(
            "commit_statuses",
            paged(
                (1..=16)
                    .map(|id| {
                        let mut row = row.clone();
                        row["id"] = json!(id);
                        row
                    })
                    .collect(),
                0,
            ),
            HEAD,
        );
        assert_eq!(category["items"].as_array().unwrap().len(), MAX_ITEMS);
        assert_eq!(category["omitted"], 8);
        assert_eq!(category["items"][0]["name"].as_str().unwrap().len(), 120);
        assert!(category["items"][0].get("body").is_none());
        assert!(category["items"][0]["attempt"].is_null());
        assert!(category["items"][0]["tested_merge_sha"].is_null());
    }

    #[test]
    fn projection_requires_exact_repository_pr_head_and_base() {
        let snapshot = json!({"version":1,"provider":"github","repository_id":12,"number":3,"head_sha":HEAD,"base_sha":BASE,"observed_at":42});
        assert!(project(&snapshot, 12, 3, HEAD, BASE).is_some());
        assert!(project(&snapshot, 13, 3, HEAD, BASE).is_none());
        assert!(project(&snapshot, 12, 4, HEAD, BASE).is_none());
        assert!(project(&snapshot, 12, 3, BASE, BASE).is_none());
        assert!(project(&snapshot, 12, 3, HEAD, HEAD).is_none());
    }

    #[test]
    fn byte_budget_marks_dropped_items_incomplete() {
        let mut snapshot = json!({});
        for kind in CATEGORIES {
            snapshot[kind] = json!({"items": vec![json!({"name": "x".repeat(1000)}); MAX_ITEMS], "complete": true, "omitted": 0});
        }
        bound(&mut snapshot);
        assert!(serde_json::to_vec(&snapshot).unwrap().len() <= MAX_SNAPSHOT_BYTES);
        assert_eq!(snapshot["retention_truncated"], true);
        assert!(
            CATEGORIES
                .iter()
                .any(|kind| snapshot[kind]["complete"] == false)
        );
    }

    #[test]
    fn source_links_reject_embedded_credentials_and_queries() {
        for url in [
            "http://example.com/path",
            "https://user:secret@example.com/path",
            "https://example.com/path?token=secret",
            "https://example.com/path#invalid/fragment",
        ] {
            assert!(source_url(&json!(url)).is_none());
        }
    }
}
