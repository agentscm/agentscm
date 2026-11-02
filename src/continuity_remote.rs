//! Explicit live continuity reads use only the checkout's saved Hub connection.
use std::path::Path;
use std::time::Duration;

use agentscm::hub::{continuity, forge_http};
use agentscm::{Error, Result};
use serde_json::{Value, json};

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}

/// Compare fresh provider observations to one exact draft. No local content is uploaded.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn observe(
    root: &Path,
    provider: &str,
    host: &str,
    repository: &str,
    source_branch: &str,
    target_branch: &str,
    expected_head: &str,
    expected_target: &str,
) -> Result<Value> {
    if !matches!(provider, "github" | "gitlab")
        || !continuity::valid_branch(source_branch)
        || !continuity::valid_branch(target_branch)
        || !continuity::valid_sha(expected_head)
        || !continuity::valid_sha(expected_target)
    {
        return Err(invalid("invalid continuity identity or draft"));
    }
    let connections = crate::client_config::load()?;
    let (connection, checkout) = crate::client_config::connection_for_checkout(&connections, root)
        .ok_or_else(|| invalid("continuity requires a saved checkout connection"))?;
    if root.canonicalize().ok().as_deref() != Some(checkout.as_path())
        || !crate::client_config::is_current_in(connection, &checkout)
        || connection.origin_host != host
        || connection.repo != repository
    {
        return Err(invalid("continuity checkout or origin identity changed"));
    }
    let token = crate::client_cli::read_token(&connection.token_file)?;
    let mut url = reqwest::Url::parse(&format!(
        "{}{}/continuity",
        connection.hub,
        crate::intent_cli::Hub::repo_path(repository)
    ))
    .map_err(|_| invalid("invalid saved Hub URL"))?;
    url.query_pairs_mut()
        .append_pair("source_branch", source_branch)
        .append_pair("target_branch", target_branch);
    let request = forge_http::client()
        .map_err(|_| invalid("cannot build continuity client"))?
        .get(url)
        .bearer_auth(token)
        .timeout(Duration::from_secs(20));
    let (status, mut observation) = forge_http::send(request, 8192)
        .await
        .map_err(|_| invalid("continuity Hub observation unavailable"))?;
    if status != 200 {
        return Err(invalid(match status {
            401 => "continuity agent token expired or revoked",
            403 => "continuity repository access denied",
            429 => "continuity Hub rate limited",
            _ => "continuity Hub observation unavailable",
        }));
    }
    if observation["version"] != 1
        || observation["provider"] != provider
        || observation["host"] != host
        || observation["repository"] != repository
        || observation["source_branch"] != source_branch
        || observation["target_branch"] != target_branch
        || observation["repository_id"]
            .as_i64()
            .is_none_or(|id| id <= 0)
    {
        return Err(invalid("continuity Hub identity or response unverified"));
    }
    if observation["availability"] != "fresh" {
        return Err(invalid(match observation["reason"].as_str() {
            Some("authentication_failed") => "continuity forge authentication failed",
            Some("permission_denied") => "continuity forge access denied",
            Some("rate_limited") => "continuity forge rate limited",
            Some("timed_out") => "continuity forge observation timed out",
            Some("connection_unavailable" | "unavailable") => {
                "continuity Hub observation unavailable"
            }
            Some("identity_or_response_unverified") => {
                "continuity Hub identity or response unverified"
            }
            _ => "continuity forge observation unverified",
        }));
    }
    let now = agentscm::hub::store::unix_now();
    if observation["observed_at"]
        .as_i64()
        .is_none_or(|at| at > now + 5 || at < now - 60)
        || observation["atomic"] != false
        || observation["authority"] != "observation_only"
    {
        return Err(invalid("continuity forge observation freshness unverified"));
    }
    let blockers = drift_blockers(&observation, expected_head, expected_target)?;
    // Recheck the local binding after the read as well; these facts are never atomic.
    if !crate::client_config::is_current_in(connection, &checkout) {
        return Err(invalid("continuity origin changed during observation"));
    }
    observation["blockers"] = json!(blockers);
    observation["remote_freshness"] = json!("fresh_observation");
    observation["publication_authorized"] = json!(false);
    Ok(observation)
}

fn drift_blockers(
    observation: &Value,
    expected_head: &str,
    expected_target: &str,
) -> Result<Vec<&'static str>> {
    let mut blockers = Vec::new();
    for (field, expected, missing, drift) in [
        (
            "source_sha",
            expected_head,
            "remote_source_missing",
            "remote_source_drift",
        ),
        (
            "target_sha",
            expected_target,
            "remote_target_missing",
            "remote_target_drift",
        ),
    ] {
        match observation.get(field) {
            Some(Value::Null) => blockers.push(missing),
            Some(Value::String(sha)) if continuity::valid_sha(sha) => {
                if !sha.eq_ignore_ascii_case(expected) {
                    blockers.push(drift);
                }
            }
            _ => return Err(invalid("continuity forge response incomplete")),
        }
    }
    Ok(blockers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_draft_comparison_reports_drift_missing_and_malformed_facts() {
        let head = "a".repeat(40);
        let target = "b".repeat(40);
        assert!(
            drift_blockers(
                &json!({"source_sha": head, "target_sha": target}),
                &head,
                &target
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(
            drift_blockers(
                &json!({"source_sha": null, "target_sha": "c".repeat(40)}),
                &head,
                &target
            )
            .unwrap(),
            vec!["remote_source_missing", "remote_target_drift"]
        );
        assert_eq!(
            drift_blockers(
                &json!({"source_sha": "c".repeat(40), "target_sha": null}),
                &head,
                &target
            )
            .unwrap(),
            vec!["remote_source_drift", "remote_target_missing"]
        );
        assert!(drift_blockers(&json!({"source_sha": head}), &head, &target).is_err());
        assert!(
            drift_blockers(
                &json!({"source_sha": "invalid", "target_sha": target}),
                &head,
                &target
            )
            .is_err()
        );
    }
}
