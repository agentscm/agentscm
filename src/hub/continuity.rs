//! Fresh, read-only forge facts for an ordinary Git checkout. No cached observation,
//! local-check attestation, publication or merge authority is returned here.
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Path, Query, State};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};

use super::HubState;
use super::http::{ApiError, Reader};
use super::intent::scope;
use super::store::unix_now;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    source_branch: String,
    target_branch: String,
}

/// A literal branch, never a revision expression or full ref.
pub fn valid_branch(name: &str) -> bool {
    super::github::valid_ref_name(name)
        && name != "HEAD"
        && name != "@"
        && !name.starts_with('-')
        && !name.starts_with("refs/")
        && !name.ends_with('.')
        && name
            .split('/')
            .all(|part| !part.starts_with('.') && !part.ends_with(".lock"))
}

pub fn valid_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

pub fn routes() -> Router<HubState> {
    Router::new().route("/v1/hub/repos/{repo}/continuity", get(show))
}

async fn show(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repository): Path<String>,
    query: Result<Query<Request>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    scope(&principal, &repository)?;
    let Query(request) = query.map_err(|_| ApiError::invalid())?;
    if !valid_branch(&request.source_branch) || !valid_branch(&request.target_branch) {
        return Err(ApiError::invalid());
    }
    // Reuse authenticated repository lookup, including its immutable repository ID.
    let (connection, repo) = super::publish::target(&state, &principal, false, true).await?;
    let unverified = |reason| {
        Json(json!({
            "version": 1, "availability": "unverified", "reason": reason,
            "provider": connection.provider, "host": connection.host,
            "repository": repo.full_name, "repository_id": repo.repository_id,
            "source_branch": request.source_branch, "target_branch": request.target_branch,
            "observed_at": null, "source_sha": null, "target_sha": null
        }))
    };
    if !state.sync.matches_forge(&connection)
        || connection.status != "enabled"
        || repo.host != connection.host
        || principal.owner.identity.provider != connection.provider
        || principal.owner.identity.host != connection.host
        || repo.repository_id <= 0
    {
        return Ok(unverified("connection_unavailable"));
    }
    let observed_at = unix_now();
    let observation = tokio::time::timeout(Duration::from_secs(15), async {
        let token = state.sync.token(&connection, repo.installation_id).await?;
        let forge = state.sync.forge(&connection)?;
        let identity = forge.repo(&token, &repo.full_name).await?;
        if !identity.is_some_and(|identity| {
            identity.repository_id == repo.repository_id && identity.full_name == repo.full_name
        }) {
            return Ok(None);
        }
        let source = forge.ref_sha(&token, &repo, &request.source_branch).await?;
        let target = forge.ref_sha(&token, &repo, &request.target_branch).await?;
        // A second identity read detects name replacement during the branch reads.
        let identity = forge.repo(&token, &repo.full_name).await?;
        if !identity.is_some_and(|identity| {
            identity.repository_id == repo.repository_id && identity.full_name == repo.full_name
        }) || source.as_deref().is_some_and(|sha| !valid_sha(sha))
            || target.as_deref().is_some_and(|sha| !valid_sha(sha))
        {
            return Ok(None);
        }
        Ok::<_, super::github::GhError>(Some((source, target)))
    })
    .await;
    match observation {
        Ok(Ok(Some((source, target)))) => Ok(Json(json!({
            "version": 1, "availability": "fresh", "provider": connection.provider,
            "host": connection.host, "repository": repo.full_name,
            "repository_id": repo.repository_id, "observed_at": observed_at,
            "source_branch": request.source_branch, "target_branch": request.target_branch,
            "source_sha": source, "target_sha": target,
            "atomic": false, "authority": "observation_only"
        }))),
        Ok(Ok(None)) => Ok(unverified("identity_or_response_unverified")),
        Ok(Err(error)) => Ok(unverified(super::provider_context::error_availability(
            &error,
        ))),
        Err(_) => Ok(unverified("timed_out")),
    }
}
