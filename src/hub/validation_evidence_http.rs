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
