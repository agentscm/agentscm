//! Read-only PR context over retained observations. No forge calls or merge authority.
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Path, Query, State};
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

use super::connection::Observation;
use super::http::{ApiError, Reader, blocking};
use super::intent::scope;
use super::store::unix_now;
use super::work::{WorkOverview, WorkOverviewQuery, fit_json_budget, scope_overlap_reasons};
use super::{HubState, Principal};
use crate::skill_workflow::supported::RunView;

const MAX_ITEMS: usize = 8;
const MAX_PATHS: usize = 4;
const RUN_SCAN: usize = 25;
pub const MAX_RESPONSE_BYTES: usize = 48 * 1024;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub number: i64,
    /// Caller-supplied comparison only; never a current provider head assertion.
    pub expected_head: Option<String>,
}

impl Request {
    fn validate(&self) -> Result<(), ApiError> {
        if self.number <= 0
            || self
                .expected_head
                .as_ref()
                .is_some_and(|head| !valid_sha(head))
        {
            return Err(ApiError::invalid());
        }
        Ok(())
    }
}

pub fn routes() -> Router<HubState> {
    Router::new().route("/v1/hub/repos/{repo}/pr-context", get(show))
}

async fn show(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
    query: Result<Query<Request>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    scope(&principal, &repo)?;
    let Query(request) = query.map_err(|_| ApiError::invalid())?;
    read(&state, &principal, request).await.map(Json)
}

/// HTTP and MCP use this same authenticated service. Reads are independent observations,
/// not an atomic provider snapshot. Never reuse a result across requesting principals.
pub async fn read(
    state: &HubState,
    principal: &Principal,
    request: Request,
) -> Result<Value, ApiError> {
    request.validate()?;
    let store = state.store.clone();
    let sync = state.sync.clone();
    let principal = principal.clone();
    let value = blocking(state, move || {
        let Some(repo) = super::sync::find_repository(
            &*store,
            &principal.owner.identity.host,
            &principal.repository,
            principal.repository_id,
        )?
        else {
            return Ok(json!({"version": 1, "availability": "not_connected"}));
        };
        let rows = store.observations(&repo.id, "pull_request")?;
        let overview = store.work_overview(
            &principal.token_id,
            &WorkOverviewQuery::default(),
            unix_now(),
        )?;
        // Reuse supported-skill evaluation. The scan is deliberately bounded and never
        // described as exhaustive, including when an unknown catalog entry was skipped.
        let runs = super::skills::views(&*store, &principal.repository, None, None, RUN_SCAN)?;
        let mut result = project(&principal.repository, &request, &rows, &overview, &runs)?;
        if result["coverage"].is_object() {
            result["coverage"]["last_sync_partial"] = json!(sync.last_pr_sync_partial(&repo.id));
        }
        if result["availability"] == "available" {
            result["repository_id"] = json!(repo.repository_id);
            let evidence: Vec<_> = [
                store.observation(&repo.id, "provider_context", "_health")?,
                store.observation(&repo.id, "provider_context", &request.number.to_string())?,
            ]
            .into_iter()
            .flatten()
            .collect();
            let enabled = store
                .connection(&repo.connection)?
                .is_some_and(|c| c.status == "enabled");
            result["provider_evidence"] =
                provider_evidence(&result, repo.repository_id, &evidence, enabled);
            for key in ["reviews", "checks", "statuses", "permissions"] {
                result["coverage"][key] = json!("see_provider_evidence");
            }
        }
        Ok(result)
    })
    .await?;
    if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > MAX_RESPONSE_BYTES) {
        return Err(ApiError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "context_too_large",
        ));
    }
    Ok(value)
}

fn valid_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn short(value: &str) -> String {
    let mut chars = value.chars();
    let mut text: String = chars.by_ref().take(240).collect();
    if chars.next().is_some() {
        text.push('…');
    }
    text
}

fn text(value: &Value, key: &str) -> String {
    short(value[key].as_str().unwrap_or_default())
}

fn sha(value: &Value, key: &str) -> Option<String> {
    value[key]
        .as_str()
        .filter(|s| valid_sha(s))
        .map(str::to_owned)
}

fn safe_url(raw: &str) -> bool {
    raw.len() <= 2048
        && reqwest::Url::parse(raw).is_ok_and(|url| {
            url.scheme() == "https" && url.username().is_empty() && url.password().is_none()
        })
}

fn files(value: &Value) -> BTreeSet<&str> {
    value["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn provider_evidence(
    context: &Value,
    repository_id: i64,
    rows: &[Observation],
    enabled: bool,
) -> Value {
    let unavailable = || json!({"availability": "unavailable"});
    if !enabled {
        return unavailable();
    }
    let Some(health) = rows
        .iter()
        .find(|row| row.key == "_health")
        .and_then(|row| serde_json::from_str::<Value>(&row.data).ok())
    else {
        return unavailable();
    };
    if health["availability"] != "available" {
        return unavailable();
    }
    let pr = &context["pull_request"];
    let (Some(number), Some(head), Some(base)) = (
        pr["number"].as_i64(),
        pr["head_sha"].as_str(),
        pr["base_sha"].as_str(),
    ) else {
        return json!({"availability": "identity_unavailable"});
    };
    let Some(row) = rows.iter().find(|row| row.key == number.to_string()) else {
        return json!({"availability": "not_collected"});
    };
    let invalidated_through = match health.get("invalidated_through") {
        None => 0, // Legacy available health has no recorded failure floor.
        Some(floor) => match floor.as_i64().filter(|floor| *floor >= 0) {
            Some(floor) => floor,
            None => return unavailable(),
        },
    };
    // Peer grant recovery cannot revive evidence collected before (or at) the
    // latest known failure. Only a newer observation of this PR can make it visible.
    if row.observed_at <= invalidated_through {
        return unavailable();
    }
    let Ok(mut snapshot) = serde_json::from_str::<Value>(&row.data) else {
        return unavailable();
    };
    snapshot["observed_at"] = json!(row.observed_at);
    let Some(mut snapshot) =
        super::provider_context::project(&snapshot, repository_id, number, head, base)
    else {
        return json!({"availability": "different_or_unknown_revision"});
    };
    // The latest installation grant observation can revoke access to older per-PR
    // evidence before its rotating refresh arrives. Requester access is still the
    // separately authenticated Hub token, not a claim of live GitHub authorization.
    for (category, grant) in [
        ("reviews", "pull_requests"),
        ("check_runs", "checks"),
        ("commit_statuses", "statuses"),
    ] {
        if health["permissions"][grant] != "available" {
            snapshot[category] = json!({"availability": "permission_unconfirmed", "complete": false,
                "items": [], "omitted": 0, "malformed": 0, "pages_read": 0, "unread_pages": null});
        }
    }
    json!({"availability": "observed", "snapshot": snapshot,
        "requester_access": "hub_token_scope", "live_requester_access": "not_revalidated"})
}

fn project(
    repository: &str,
    request: &Request,
    rows: &[Observation],
    overview: &WorkOverview,
    runs: &[RunView],
) -> super::Result<Value> {
    let pulls: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            let value: Value = serde_json::from_str(&row.data).ok()?;
            (value["state"] == "open").then_some((row, value))
        })
        .collect();
    let mut coverage = json!({
            "pull_requests": "retained_only", "pull_requests_complete": false,
            "retained_pr_scan_saturated": rows.len() >= 1000, "last_sync_partial": null, "files_partial": true,
            "work_complete": overview.current_complete, "work_observed_at": overview.until,
            "workflows": "newest_25_runs_only", "workflows_complete": false,
            "workflow_scan_limit": RUN_SCAN, "atomic_snapshot": false,
            "local_source": "not_observed", "base_revision": "not_retained",
            "reviews": "not_collected", "checks": "not_collected",
            "permissions": "not_inspected", "session_baseline": "use_existing_catchup"
    });
    let Some((row, pr)) = pulls
        .iter()
        .find(|(_, p)| p["number"].as_i64() == Some(request.number))
    else {
        return Ok(json!({"version": 1, "availability": "not_observed", "coverage": coverage}));
    };
    let head = sha(pr, "head_sha");
    coverage["base_revision"] = json!(if sha(pr, "base_sha").is_some() {
        "provider_observed"
    } else {
        "not_retained"
    });
    let head_relation = match (&request.expected_head, &head) {
        (Some(expected), Some(current)) if expected.eq_ignore_ascii_case(current) => "same",
        (Some(_), Some(_)) => "changed",
        _ => "unknown",
    };
    let paths = files(pr);
    coverage["files_partial"] = json!(pr["files_truncated"] != false || paths.is_empty());
    let mut pr_overlaps = Vec::new();
    for (other_row, other) in &pulls {
        if other["number"] == pr["number"] {
            continue;
        }
        let other_paths = files(other);
        let shared: Vec<_> = other_paths.intersection(&paths).map(|p| short(p)).collect();
        if !shared.is_empty() {
            pr_overlaps.push(json!({
                "number": other["number"], "title": text(other, "title"), "author": text(&other["author"], "login"),
                "head_sha": sha(other, "head_sha"), "draft": other["draft"] == true,
                "observed_at_ms": other_row.observed_at,
                "paths": shared.iter().take(MAX_PATHS).collect::<Vec<_>>(),
                "paths_omitted": shared.len().saturating_sub(MAX_PATHS),
                "files_partial": other["files_truncated"] != false || other_paths.is_empty(),
            }));
        }
    }
    let pr_total = pr_overlaps.len();
    pr_overlaps.truncate(MAX_ITEMS);
    fit_json_budget(&mut pr_overlaps, 8 * 1024)?;
    let pr_omitted = pr_total - pr_overlaps.len();
    let path_vec: Vec<String> = paths.iter().map(|p| (*p).to_owned()).collect();
    let mut work_overlaps = Vec::new();
    for work in &overview.current {
        let reasons = scope_overlap_reasons(&path_vec, &work.scope);
        if reasons.is_empty() {
            continue;
        }
        work_overlaps.push(json!({
            "work_id": work.id, "title": short(&work.title),
            "agent": short(&work.lead_agent), "owner": short(&work.owner_login),
            "updated_at": work.updated_at, "claim_status": work.claim_status,
            "blocker": work.blocker.as_ref().map(|c| short(&c.summary)),
            "next_action": work.next_action.as_deref().map(short),
            "paths": reasons.iter().map(|r| short(&r.your_path)).collect::<BTreeSet<_>>().into_iter().take(MAX_PATHS).collect::<Vec<_>>(),
            "paths_partial": reasons.len() > MAX_PATHS,
        }));
    }
    let work_total = work_overlaps.len();
    work_overlaps.truncate(MAX_ITEMS);
    fit_json_budget(&mut work_overlaps, 8 * 1024)?;
    let work_omitted = work_total - work_overlaps.len();
    let mut workflows = Vec::new();
    for run in runs
        .iter()
        .filter(|r| r.repo == repository && r.pull_request == Some(request.number as u64))
    {
        let applicable = head
            .as_ref()
            .is_some_and(|h| valid_sha(&run.head_sha) && h.eq_ignore_ascii_case(&run.head_sha));
        workflows.push(json!({
            "run_id": run.run_id, "head_sha": if valid_sha(&run.head_sha) { Some(&run.head_sha) } else { None },
            "applicability": if applicable { "matches_observed_head" } else if head.is_some() && valid_sha(&run.head_sha) { "earlier_or_other_head" } else { "unknown" },
            "current_gate": run.current_gate, "next_action": short(&run.next_action),
            "blocks_finish": run.blocks_finish, "updated_at": run.updated_at,
            "basis": "self_reported",
        }));
    }
    let workflow_total = workflows.len();
    workflows.truncate(MAX_ITEMS);
    fit_json_budget(&mut workflows, 8 * 1024)?;
    let workflow_omitted = workflow_total - workflows.len();
    Ok(json!({
        "version": 1, "availability": "available", "repository": repository,
        "pull_request": {"number": request.number, "title": text(pr, "title"), "author": text(&pr["author"], "login"),
            "head_ref": text(pr, "head_ref"), "head_sha": head, "base_ref": text(pr, "base_ref"),
            "base_sha": sha(pr, "base_sha"), "draft": pr["draft"] == true,
            "url": pr["html_url"].as_str().filter(|url| safe_url(url)),
            "observed_at_ms": row.observed_at, "basis": "provider_observed"},
        "expected_head": request.expected_head, "head_relation": head_relation,
        "pull_request_overlaps": pr_overlaps, "pull_request_overlaps_omitted": pr_omitted,
        "work_overlaps": work_overlaps, "work_overlaps_omitted": work_omitted,
        "workflows": workflows, "workflows_omitted": workflow_omitted,
        "coverage": coverage,
        "next_actions": {"catchup": "#activity", "coordination": "#inbox", "skills": "#skills/runs"}
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn overview() -> WorkOverview {
        WorkOverview {
            recent_since: 0,
            until: 100,
            current: vec![],
            recent: vec![],
            overlaps: vec![],
            current_complete: true,
            recent_complete: true,
            overlaps_complete: true,
            history_retention_gap: false,
        }
    }
    fn row(number: i64, head: &str, paths: &[&str]) -> Observation {
        Observation {
            kind: "pull_request".into(),
            key: number.to_string(),
            observed_at: 100_000,
            data: json!({"number":number,"title":"Synthetic PR","state":"open","draft":false,
                "head_sha":head,"head_ref":"feature","base_ref":"main","files":paths,
                "files_truncated":false})
            .to_string(),
        }
    }
    #[test]
    fn provider_failure_floor_is_inclusive_and_rejects_malformed_bounds() {
        let head = "a".repeat(40);
        let base = "b".repeat(40);
        let context = json!({"pull_request": {"number": 1, "head_sha": head, "base_sha": base}});
        let mut health = json!({"availability": "available", "permissions": {
            "pull_requests": "available", "checks": "available", "statuses": "available"}});
        let snapshot = json!({"version": 1, "provider": "github", "repository_id": 100,
            "number": 1, "head_sha": head, "base_sha": base});
        let mut rows = vec![
            Observation {
                kind: "provider_context".into(),
                key: "_health".into(),
                data: health.to_string(),
                observed_at: 200,
            },
            Observation {
                kind: "provider_context".into(),
                key: "1".into(),
                data: snapshot.to_string(),
                observed_at: 100,
            },
        ];
        assert_eq!(
            provider_evidence(&context, 100, &rows, true)["availability"],
            "observed"
        );
        for (floor, expected) in [
            (json!(99), "observed"),
            (json!(100), "unavailable"),
            (json!(101), "unavailable"),
            (json!("100"), "unavailable"),
            (json!(-1), "unavailable"),
        ] {
            health["invalidated_through"] = floor;
            rows[0].data = health.to_string();
            assert_eq!(
                provider_evidence(&context, 100, &rows, true)["availability"],
                expected
            );
        }
    }

    #[test]
    fn head_comparison_does_not_grant_authority_and_overlap_is_bounded() {
        let rows = vec![
            row(1, &"a".repeat(40), &["a.rs"]),
            row(2, &"b".repeat(40), &["a.rs"]),
        ];
        let result = project(
            "acme/widgets",
            &Request {
                number: 1,
                expected_head: Some("b".repeat(40)),
            },
            &rows,
            &overview(),
            &[],
        )
        .unwrap();
        assert_eq!(result["head_relation"], "changed");
        assert_eq!(result["pull_request"]["observed_at_ms"], 100_000);
        assert_eq!(result["pull_request_overlaps"][0]["number"], 2);
        assert_eq!(result["coverage"]["reviews"], "not_collected");
        assert_eq!(result["coverage"]["permissions"], "not_inspected");
        assert_eq!(result["coverage"]["atomic_snapshot"], false);
        assert!(serde_json::to_vec(&result).unwrap().len() < MAX_RESPONSE_BYTES);
    }
    #[test]
    fn unknown_heads_and_unobserved_prs_are_not_clean() {
        let rows = vec![row(1, "", &[])];
        let result = project(
            "acme/widgets",
            &Request {
                number: 1,
                expected_head: None,
            },
            &rows,
            &overview(),
            &[],
        )
        .unwrap();
        assert_eq!(result["head_relation"], "unknown");
        assert!(result["pull_request"]["head_sha"].is_null());
        let missing = project(
            "acme/widgets",
            &Request {
                number: 2,
                expected_head: None,
            },
            &rows,
            &overview(),
            &[],
        )
        .unwrap();
        assert_eq!(missing["availability"], "not_observed");
        assert!(
            Request {
                number: 0,
                expected_head: None
            }
            .validate()
            .is_err()
        );
        assert!(
            Request {
                number: 1,
                expected_head: Some("main".into())
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn exact_paths_closed_rows_and_output_limits() {
        let mut rows = vec![row(
            1,
            &"a".repeat(40),
            &["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs"],
        )];
        rows.extend((2..15).map(|n| {
            row(
                n,
                &"b".repeat(40),
                &["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs"],
            )
        }));
        rows.push(row(20, &"c".repeat(40), &["src/a.rs.old"]));
        let mut closed = row(21, &"c".repeat(40), &["src/a.rs"]);
        closed.data = closed.data.replace("\"open\"", "\"closed\"");
        rows.push(closed);
        let result = project(
            "acme/widgets",
            &Request {
                number: 1,
                expected_head: None,
            },
            &rows,
            &overview(),
            &[],
        )
        .unwrap();
        assert_eq!(result["pull_request_overlaps"].as_array().unwrap().len(), 8);
        assert_eq!(result["pull_request_overlaps_omitted"], 5);
        assert_eq!(result["pull_request_overlaps"][0]["paths_omitted"], 1);
    }
    #[test]
    fn workflow_link_requires_pr_number_and_exact_head_not_branch() {
        let mut run: RunView = serde_json::from_value(json!({
            "run_id":"synthetic", "repo":"acme/widgets", "branch":"feature",
            "pull_request":1, "head_sha":"b".repeat(40), "state":"open",
            "skill":{"name":"pr-gate","workflow_version":1,"sha256":"a".repeat(64)},
            "params":{}, "current_gate":"review", "next_action":"Review this head",
            "blocks_finish":true,"gates":[],"stale_evidence":[],"updated_at":100
        }))
        .unwrap();
        let rows = vec![row(1, &"a".repeat(40), &[])];
        let request = Request {
            number: 1,
            expected_head: None,
        };
        let result = project("acme/widgets", &request, &rows, &overview(), &[run.clone()]).unwrap();
        assert_eq!(
            result["workflows"][0]["applicability"],
            "earlier_or_other_head"
        );
        run.head_sha = "a".repeat(40);
        let result = project("acme/widgets", &request, &rows, &overview(), &[run.clone()]).unwrap();
        assert_eq!(
            result["workflows"][0]["applicability"],
            "matches_observed_head"
        );
        assert_eq!(result["workflows"][0]["basis"], "self_reported");
        run.pull_request = None;
        assert_eq!(
            project("acme/widgets", &request, &rows, &overview(), &[run]).unwrap()["workflows"],
            json!([])
        );
    }
    #[test]
    fn missing_pr_retains_known_scan_coverage() {
        let rows: Vec<_> = (1..=1000)
            .map(|n| row(n, &"a".repeat(40), &["src/a.rs"]))
            .collect();
        let result = project(
            "acme/widgets",
            &Request {
                number: 1001,
                expected_head: None,
            },
            &rows,
            &overview(),
            &[],
        )
        .unwrap();
        assert_eq!(result["availability"], "not_observed");
        assert_eq!(result["coverage"]["retained_pr_scan_saturated"], true);
        assert_eq!(result["coverage"]["pull_requests_complete"], false);
        assert!(result["coverage"]["last_sync_partial"].is_null());
    }
    #[test]
    fn multibyte_and_escaped_paths_fit_without_losing_omission_counts() {
        let path = "界".repeat(240);
        let mut rows: Vec<_> = (1..20).map(|n| row(n, &"a".repeat(40), &[&path])).collect();
        for row in &mut rows {
            let mut data: Value = serde_json::from_str(&row.data).unwrap();
            data["title"] = json!("\u{1}".repeat(240));
            data["html_url"] = json!("javascript:alert(1)");
            row.data = data.to_string();
        }
        let result = project(
            "acme/widgets",
            &Request {
                number: 1,
                expected_head: None,
            },
            &rows,
            &overview(),
            &[],
        )
        .unwrap();
        assert!(serde_json::to_vec(&result).unwrap().len() < MAX_RESPONSE_BYTES);
        let kept = result["pull_request_overlaps"].as_array().unwrap().len();
        assert_eq!(
            kept as u64 + result["pull_request_overlaps_omitted"].as_u64().unwrap(),
            18
        );
        assert!(result["pull_request"]["url"].is_null());
        assert_eq!(result["coverage"]["pull_requests_complete"], false);
    }
}
