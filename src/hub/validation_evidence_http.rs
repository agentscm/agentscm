//! Explicit, bounded evidence intake. Receipt means durable storage, never verification.
use super::connection::{ConnectedRepository, Connection};
#[cfg(unix)]
use super::forge_check::merge_group::ruleset_requirement;
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::login::HumanSession;
use super::store::unix_now;
use super::validation_evidence::{
    self, ValidationEvidencePage, ValidationEvidenceReceipt, ValidationSummaryConfig,
};
use super::{HubError, HubState};
use crate::local_validation::bundle::{MAX_BUNDLE_BYTES, ValidationBundleV1};
use axum::http::StatusCode;
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

/// The forge check module is Unix-only; elsewhere the binding is not compared.
#[cfg(not(unix))]
fn ruleset_requirement(_: &Value, _: &str, _: Option<i64>) -> &'static str {
    "unknown"
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/admin/repos/{repo}/validation-summary",
            post(configure),
        )
        .route(
            "/v1/hub/admin/repos/{repo}/connection",
            get(transfer_preview).post(transfer),
        )
        .route(
            "/v1/hub/repos/{repo}/validation-evidence/{proposal}",
            get(history)
                .post(submit)
                .layer(DefaultBodyLimit::max(MAX_BUNDLE_BYTES)),
        )
        .route(
            "/v1/hub/repos/{repo}/validation-evidence/{proposal}/{generation}",
            get(read),
        )
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configure {
    enabled: bool,
}
async fn configure(
    State(state): State<HubState>,
    human: HumanSession,
    Path(repo): Path<String>,
    JsonBody(body): JsonBody<Configure>,
) -> Result<Json<Value>, ApiError> {
    human.admin()?;
    let store = state.store.clone();
    let (row, conn) = blocking(&state, move || {
        let row = store.repository_row(&repo)?.ok_or(HubError::Unauthorized)?;
        let conn = store
            .connection(&row.connection)?
            .ok_or(HubError::Unauthorized)?;
        Ok((row, conn))
    })
    .await?;
    let publisher = publisher(&state, &conn, &row).await;
    // Summaries publish only commit statuses and one comment. Without `statuses: write` every
    // verdict is refused and required checks stay expected, so refuse to enable; grant nothing.
    if body.enabled
        && publisher["permissions"]["statuses"]
            .as_str()
            .is_some_and(|granted| granted != "write")
    {
        return Err(
            ApiError::new(StatusCode::FORBIDDEN, "forge_permission_missing")
                .with_message(format!(
                    "{} lacks statuses: write on {}; grant it to that App installation, \
                     or connect the repository through an App that has it",
                    publisher["identity"]
                        .as_str()
                        .unwrap_or("the connected App"),
                    row.full_name
                ))
                .with_field("publisher", publisher)
                .with_field("last_refusal", last_refusal(&row.id).into()),
        );
    }
    let config = ValidationSummaryConfig {
        repository: row.id.clone(),
        connection: row.connection.clone(),
        enabled: body.enabled,
    };
    let store = state.store.clone();
    let stored = config.clone();
    blocking(&state, move || store.configure_validation_summary(&stored)).await?;
    let mut reply = serde_json::to_value(&config).map_err(|_| ApiError::invalid())?;
    reply["publisher"] = publisher;
    reply["last_refusal"] = last_refusal(&row.id).into();
    Ok(Json(reply))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferTarget {
    to: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Transfer {
    to: String,
    expect_from: String,
}

const TRANSFER_WARNING: &str = "The new App cannot edit or remove statuses or the summary \
    comment the old App posted; clear or accept them on GitHub.";
const SUMMARY_WARNING: &str = "Validation summaries are disabled by the transfer; enable them \
    again under the new connection.";

/// A checked move of `row` to `to`: the reply both routes send, and what the move needs.
struct TransferPlan {
    row: ConnectedRepository,
    to: String,
    installation: i64,
    reply: Value,
}

/// Reads everything a transfer depends on and changes nothing. `to` must be another GitHub
/// App connection on the repository's host whose installation reaches this repository id.
async fn plan_transfer(
    state: &HubState,
    repo: String,
    to: String,
) -> Result<TransferPlan, ApiError> {
    let store = state.store.clone();
    let (row, from, target, enabled) = blocking(state, move || {
        let row = store.repository_row(&repo)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let from = store.connection(&row.connection)?;
        let target = store.connection(&to)?;
        let enabled = store
            .validation_summary_config(&row.id)?
            .is_some_and(|c| c.enabled && c.connection == row.connection);
        Ok(Some((row, from, target, enabled)))
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    let target = target
        .filter(|t| {
            t.kind == "app"
                && t.provider == "github"
                && t.host == row.host
                && t.id != row.connection
                && state.sync.matches_forge(t)
        })
        .ok_or_else(|| {
            ApiError::invalid()
                .with_message("to must name another GitHub App connection on the repository's host")
        })?;
    let unreachable = || {
        ApiError::new(StatusCode::FORBIDDEN, "forge_permission_missing").with_message(format!(
            "{} is not installed on {}; install it there first",
            target.app_slug.as_deref().unwrap_or("the target App"),
            row.full_name
        ))
    };
    let api = state.sync.api().map_err(super::sync::gh_to_hub)?;
    let jwt = state
        .sync
        .jwt(&target)
        .await
        .map_err(super::sync::gh_to_hub)?;
    let installation = api
        .repo_installation(&jwt, &row.full_name)
        .await
        .map_err(super::sync::gh_to_hub)?
        .ok_or_else(unreachable)?;
    // The name may since belong to another repository: only the same repository id moves.
    let token = state
        .sync
        .token(&target, Some(installation))
        .await
        .map_err(super::sync::gh_to_hub)?;
    let seen = api
        .repo(&token, &row.full_name)
        .await
        .map_err(super::sync::gh_to_hub)?;
    if seen.is_none_or(|seen| seen.repository_id != row.repository_id) {
        return Err(unreachable());
    }
    let moved = ConnectedRepository {
        connection: target.id.clone(),
        installation_id: Some(installation),
        ..row.clone()
    };
    let publisher = publisher(state, &target, &moved).await;
    let store = state.store.clone();
    let (id, current, to) = (row.id.clone(), row.connection.clone(), target.id.clone());
    let counts = blocking(state, move || {
        store.transfer_repository(&id, &current, &to, Some(installation), "", false)
    })
    .await?;
    let mut blocked = Vec::new();
    if enabled && publisher["permissions"]["statuses"] != "write" {
        blocked.push("statuses_write_missing");
    }
    if counts.mid_write > 0 {
        blocked.push("write_in_progress");
    }
    let mut warnings = vec![TRANSFER_WARNING];
    if enabled {
        warnings.push(SUMMARY_WARNING);
    }
    let reply = json!({
        "repository": {"id": row.id, "full_name": row.full_name},
        "from": {"connection": row.connection, "kind": from.as_ref().map(|c| &c.kind),
            "app_slug": from.as_ref().and_then(|c| c.app_slug.as_ref())},
        "to": {"connection": target.id, "app_slug": target.app_slug,
            "installation_id": installation},
        "publisher": publisher,
        "validation_summary_enabled": enabled,
        "resets": {"outbound": counts.outbound, "forge_checks": counts.forge_checks,
            "merge_group_posts": counts.merge_group_posts, "publications": counts.publications},
        "blocked": blocked, "warnings": warnings, "transferred": false,
    });
    Ok(TransferPlan {
        row,
        to: target.id,
        installation,
        reply,
    })
}

async fn transfer_preview(
    State(state): State<HubState>,
    human: HumanSession,
    Path(repo): Path<String>,
    Query(query): Query<TransferTarget>,
) -> Result<Json<Value>, ApiError> {
    human.admin()?;
    Ok(Json(plan_transfer(&state, repo, query.to).await?.reply))
}

fn connection_changed() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "already_connected")
        .with_message("the repository's connection changed; preview the transfer again")
}

fn write_in_progress() -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "publication_conflict").with_message(
        "a validation publication or forge check is mid-write on the repository; retry once \
         it settles",
    )
}

/// Moves one repository to another App connection. `expect_from` must still be its
/// connection; nothing else on either connection changes.
async fn transfer(
    State(state): State<HubState>,
    human: HumanSession,
    Path(repo): Path<String>,
    JsonBody(body): JsonBody<Transfer>,
) -> Result<Json<Value>, ApiError> {
    let actor = human.admin()?.user.id.clone();
    let mut plan = plan_transfer(&state, repo, body.to).await?;
    if plan.row.connection != body.expect_from {
        return Err(connection_changed());
    }
    let blocked = &plan.reply["blocked"];
    if blocked
        .as_array()
        .is_some_and(|b| b.contains(&json!("statuses_write_missing")))
    {
        return Err(
            ApiError::new(StatusCode::FORBIDDEN, "forge_permission_missing")
                .with_message(format!(
                    "{} lacks statuses: write on {}, and validation summaries are enabled; \
                     grant it, or disable summaries first",
                    plan.reply["publisher"]["identity"]
                        .as_str()
                        .unwrap_or("the target App"),
                    plan.row.full_name
                ))
                .with_field("publisher", plan.reply["publisher"].clone()),
        );
    }
    if blocked.as_array().is_some_and(|b| !b.is_empty()) {
        return Err(write_in_progress());
    }
    // Polls, webhook jobs, publication runs and deployment records take this lock and then
    // re-read the repository row, so one that waited behind the move stops rather than writes
    // with the old App. Work already holding the lock finishes before the move commits.
    let _guard = state.sync.lock_repo(&plan.row.id).await;
    let store = state.store.clone();
    let (id, from, to, installation) = (
        plan.row.id.clone(),
        body.expect_from,
        plan.to.clone(),
        plan.installation,
    );
    let done = blocking(&state, move || {
        store.transfer_repository(&id, &from, &to, Some(installation), &actor, true)
    })
    .await?;
    if done.connection_changed {
        return Err(connection_changed());
    }
    if !done.moved {
        return Err(write_in_progress());
    }
    #[cfg(unix)]
    super::validation_summary::forget_refusal(&plan.row.id);
    plan.reply["transferred"] = json!(true);
    Ok(Json(plan.reply))
}

/// The publication controller is Unix-only, so elsewhere nothing has been refused.
fn last_refusal(repo: &str) -> Option<Value> {
    #[cfg(unix)]
    return super::validation_summary::last_refusal(repo);
    #[cfg(not(unix))]
    {
        let _ = repo;
        None
    }
}

/// What GitHub says the connected credential may publish, read with existing credentials only.
/// It never asks for or grants a permission. Classic branch protection is not read (it needs
/// administration access), so operators compare it by hand against `app_id`.
async fn publisher(state: &HubState, conn: &Connection, row: &ConnectedRepository) -> Value {
    let identity = match conn.kind.as_str() {
        "app" => conn.app_slug.as_ref().map(|slug| format!("{slug}[bot]")),
        _ => conn.acts_as.clone(),
    };
    let mut out = json!({"connection": conn.id, "kind": conn.kind, "app_id": conn.app_id,
        "identity": identity, "permissions": "not_inspected", "required_contexts": "unknown",
        "classic_protection": "not_read", "warnings": []});
    // Only canonical GitHub, as publication requires: never send another forge's token here.
    if conn.provider != "github" || !state.sync.matches_forge(conn) {
        return out;
    }
    let Ok(api) = state.sync.api() else {
        return out;
    };
    let mut warnings = Vec::new();
    if conn.kind == "app" {
        let installation = match (row.installation_id, state.sync.jwt(conn).await) {
            (Some(id), Ok(jwt)) => api.installation(&jwt, id).await.ok().flatten(),
            _ => None,
        };
        out["permissions"] = match installation {
            Some(found) => {
                let granted = |name: &str| found["permissions"][name].as_str().unwrap_or("none");
                if granted("pull_requests") != "write" && granted("issues") != "write" {
                    warnings.push("comment_permission_missing");
                }
                json!({"statuses": granted("statuses"), "pull_requests": granted("pull_requests"),
                    "issues": granted("issues"), "contents": granted("contents")})
            }
            None => {
                warnings.push("permissions_unreadable");
                json!("unknown")
            }
        };
    }
    let rules = match state.sync.token(conn, row.installation_id).await {
        Ok(token) => match api.default_branch(&token, &row.full_name).await {
            Ok(branch) => api
                .branch_rules(&token, &row.full_name, &branch)
                .await
                .ok()
                .flatten(),
            Err(_) => None,
        },
        Err(_) => None,
    };
    match rules {
        Some(rules) => {
            let mut contexts: Vec<&str> = rules
                .as_array()
                .into_iter()
                .flatten()
                .filter(|rule| rule["type"] == "required_status_checks")
                .flat_map(|rule| {
                    rule["parameters"]["required_status_checks"]
                        .as_array()
                        .into_iter()
                        .flatten()
                })
                .filter_map(|check| check["context"].as_str())
                .collect();
            contexts.sort_unstable();
            contexts.dedup();
            let bound: serde_json::Map<String, Value> = contexts
                .into_iter()
                .map(|context| {
                    let source = ruleset_requirement(&rules, context, conn.app_id);
                    if source == "required_from_another_source" {
                        warnings.push("required_context_bound_to_another_source");
                    }
                    (context.to_owned(), json!(source))
                })
                .collect();
            out["required_contexts"] = Value::Object(bound);
        }
        None => warnings.push("required_rules_unreadable"),
    }
    warnings.dedup();
    out["warnings"] = json!(warnings);
    out
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryQuery {
    after: Option<i64>,
    limit: Option<usize>,
}
async fn history(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path((repo, proposal)): Path<(String, i64)>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<ValidationEvidencePage>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.validation_evidence_history(
                &principal.token_id,
                &repo,
                proposal,
                query.after.unwrap_or(0),
                query.limit.unwrap_or(50),
                unix_now(),
            )
        })
        .await?,
    ))
}
async fn submit(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path((repo, proposal)): Path<(String, i64)>,
    body: Bytes,
) -> Result<Json<ValidationEvidenceReceipt>, ApiError> {
    if principal.read_only {
        return Err(ApiError::unauthorized());
    }
    let store = state.store.clone();
    let objects = state.objects.clone();
    Ok(Json(
        blocking(&state, move || {
            // Current token and exact connected repository permission before parsing or object IO.
            store.validation_evidence_history(
                &principal.token_id,
                &repo,
                proposal,
                0,
                1,
                unix_now(),
            )?;
            let row = store.repository_row(&repo)?.ok_or(HubError::Unauthorized)?;
            if !store
                .validation_summary_config(&repo)?
                .is_some_and(|c| c.enabled && c.connection == row.connection)
            {
                return Err(HubError::Invalid(
                    "Hub validation summary is not enabled".into(),
                ));
            }
            let bundle = ValidationBundleV1::from_slice(&body)?;
            let durable = validation_evidence::persist_bundle(
                &*objects,
                &repo,
                &format!("{}/{}", row.host, row.full_name),
                &bundle,
            )?;
            // Transactionally rechecks token, repository binding and configuration after object IO.
            store.receive_validation_evidence(&principal.token_id, proposal, &durable, unix_now())
        })
        .await?,
    ))
}
async fn read(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path((repo, proposal, generation)): Path<(String, i64, i64)>,
) -> Result<Json<ValidationBundleV1>, ApiError> {
    if generation <= 0 {
        return Err(ApiError::invalid());
    }
    let store = state.store.clone();
    let objects = state.objects.clone();
    let bundle = blocking(&state, move || {
        // Exact bounded generation lookup through authorized recorded history, never a digest supplied by the caller.
        let page = store.validation_evidence_history(
            &principal.token_id,
            &repo,
            proposal,
            generation - 1,
            1,
            unix_now(),
        )?;
        match page.entries.first().filter(|r| r.generation == generation) {
            Some(receipt) => validation_evidence::read_bundle(&*objects, receipt).map(Some),
            None => Ok(None),
        }
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
    Ok(Json(bundle))
}
