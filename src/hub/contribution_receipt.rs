//! Small deterministic receipts. These are observations, never execution or merge authority.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::super::http::blocking;
use super::super::store::{Store, unix_now};
use super::super::value_metrics::ValueMetricsQuery;
use super::super::{HubState, Principal, Result};

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Mode {
    Automatic,
    #[default]
    RequestedOnly,
    Disabled,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsArgs {
    session: Option<String>,
    mode: Option<Mode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptArgs {
    work_id: String,
    session: Option<String>,
}

fn identity(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{:x}", hash.finalize())
}

fn mode(store: &dyn Store, token: &str, session: &str) -> Result<Mode> {
    // Corrupt preferences fail closed to explicit retrieval, never automatic disclosure.
    super::super::intent::text("receipt session", session, 255)?;
    Ok(store
        .receipt_mode_get(token, session, unix_now())?
        .and_then(|raw| serde_json::from_value(Value::String(raw)).ok())
        .unwrap_or_default())
}

fn source(item: &super::super::work::WorkItem) -> Option<&str> {
    let mut commits = item
        .current
        .as_ref()?
        .source_refs
        .iter()
        .filter_map(|r| match r {
            super::super::work::SourceRef::Commit { oid, .. } => Some(oid.as_str()),
            _ => None,
        });
    let first = commits.next()?;
    commits.all(|commit| commit == first).then_some(first)
}
fn work_projection(
    store: &dyn Store,
    token: &str,
    item: &super::super::work::WorkItem,
    now: i64,
) -> Result<Value> {
    let mut projection = serde_json::to_value(store.value_metrics(
        token,
        &ValueMetricsQuery {
            work_id: Some(item.id.clone()),
            since: Some(item.created_at.max(now.saturating_sub(90 * 86_400))),
            limit: Some(20),
            ..Default::default()
        },
        now,
    )?)
    .map_err(|_| super::super::HubError::Unavailable("receipt encoding failed".into()))?;
    projection["receipt_source"] = json!(source(item));
    Ok(projection)
}

pub(super) async fn call(
    state: &HubState,
    principal: &Principal,
    name: &str,
    arguments: Option<&Value>,
) -> Value {
    let store = state.store.clone();
    let token = principal.token_id.clone();
    if name == "agentscm_receipt_settings" {
        let args: SettingsArgs = match super::try_args(arguments) {
            Ok(args) => args,
            Err(error) => return super::invalid_args(name, &error),
        };
        let read = blocking(state, move || {
            if let Some(session) = &args.session {
                super::super::intent::text("receipt session", session, 255)?;
            }
            let session = store.session_name_for(&token, args.session.as_deref(), unix_now())?;
            if let Some(mode) = args.mode {
                let value = match mode {
                    Mode::Automatic => "automatic",
                    Mode::RequestedOnly => "requested_only",
                    Mode::Disabled => "disabled",
                };
                store.receipt_mode_set(&token, &session, value, unix_now())?;
            }
            Ok(json!({"session":session,"mode":mode(store.as_ref(), &token, &session)?}))
        })
        .await;
        return match read {
            Ok(value) => super::structured(&value),
            Err(error) => super::work_failure(&error),
        };
    }
    let args: ReceiptArgs = match super::try_args(arguments) {
        Ok(args) => args,
        Err(error) => return super::invalid_args(name, &error),
    };
    let repo = principal.repository.clone();
    let read = blocking(state, move || {
        let now = unix_now();
        if let Some(session) = &args.session {
            super::super::intent::text("receipt session", session, 255)?;
        }
        let session = store.session_name_for(&token, args.session.as_deref(), now)?;
        if mode(store.as_ref(), &token, &session)? == Mode::Disabled {
            return Ok(None);
        }
        let Some(item) = store.work_get(&token, &args.work_id, now)? else {
            return Ok(None);
        };
        let projection = work_projection(store.as_ref(), &token, &item, now)?;
        Ok(Some(preview(
            &repo,
            &item.id,
            item.generation,
            "current",
            &projection,
        )))
    })
    .await;
    match read {
        Ok(Some(receipt)) => response(receipt),
        Ok(None) => super::refused("receipt unavailable or disabled"),
        Err(error) => super::work_failure(&error),
    }
}

/// Automatic receipts run only after a supported substantive operation succeeded. Reads never
/// acknowledge context; this marker records an announcement, not review or application.
pub(super) async fn append(
    state: &HubState,
    principal: &Principal,
    name: &str,
    arguments: Option<&Value>,
    result: &mut Value,
) {
    if result["isError"] == true || principal.read_only {
        return;
    }
    let phase = match name {
        "agentscm_work_checkpoint" if arguments.is_some_and(|a| a["kind"] == "finish") => {
            "completion"
        }
        "agentscm_skill_evidence"
            if arguments.is_some_and(|a| {
                matches!(a["kind"].as_str(), Some("revision_tests" | "hosted_ci"))
            }) =>
        {
            "check"
        }
        "agentscm_skill_evidence" if arguments.is_some_and(|a| a["kind"] == "human_approval") => {
            "pre_merge"
        }
        "agentscm_work_catchup" => "merge_update",
        _ => return,
    };
    let args = arguments.cloned().unwrap_or_else(|| json!({}));
    let skill_evidence = name == "agentscm_skill_evidence";
    let store = state.store.clone();
    let token = principal.token_id.clone();
    let repo = principal.repository.clone();
    let checkpoint_id = result["structuredContent"]["item"]["id"]
        .as_str()
        .map(str::to_owned);
    let merge_heads: Vec<String> = result["structuredContent"]["merges"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["head_commit"].as_str().map(str::to_owned))
        .take(50)
        .collect();
    let read = blocking(state, move || {
        let now = unix_now();
        let requested_session = args["session"].as_str().or_else(|| {
            args["subscriber"]
                .as_str()
                .and_then(|s| s.strip_prefix("agent:"))
        });
        if let Some(session) = requested_session {
            super::super::intent::text("receipt session", session, 255)?;
        }
        let session = store.session_name_for(&token, requested_session, now)?;
        if mode(store.as_ref(), &token, &session)? != Mode::Automatic {
            return Ok(Vec::new());
        }
        let mut items = Vec::new();
        if phase == "merge_update" {
            if merge_heads.is_empty() {
                return Ok(Vec::new());
            }
            for id in store.receipt_work_ids(&token, &session, now)? {
                if let Some(item) = store.work_get(&token, &id, now)?
                    && source(&item).is_some_and(|head| merge_heads.iter().any(|m| m == head))
                {
                    items.push(item);
                }
            }
        } else if let Some(id) = checkpoint_id {
            if let Some(item) = store.work_get(&token, &id, now)? {
                items.push(item);
            }
        } else if let Some(item) = store.capture_work_for_session(&token, &session, now)? {
            items.push(item);
        }
        let mut receipts = Vec::new();
        for item in items {
            if !item
                .participants
                .iter()
                .any(|p| p.token == token && p.session == session)
            {
                continue;
            }
            if skill_evidence
                && (args["branch"].as_str() != item.branch.as_deref()
                    || args["head_sha"].as_str() != source(&item))
            {
                continue;
            }
            let projection = work_projection(store.as_ref(), &token, &item, now)?;
            let receipt = preview(&repo, &item.id, item.generation, phase, &projection);
            if phase == "merge_update" && receipt["phase"] != "provider_observed_merge" {
                continue;
            }
            store.receipt_work_remember(&token, &session, &item.id, now)?;
            let announcement = announcement_identity(&receipt);
            if store.receipt_announce(
                &token,
                &session,
                &item.id,
                item.generation,
                &announcement,
                now,
            )? {
                receipts.push(receipt);
            }
            if receipts.len() == 3 {
                break;
            }
        }
        Ok(receipts)
    })
    .await;
    match read {
        Ok(receipts) if !receipts.is_empty() => {
            if let Some(content) = result["content"].as_array_mut() {
                for receipt in &receipts {
                    content.push(json!({"type":"text","text":receipt["text"]}));
                }
            }
            if let Some(content) = result["structuredContent"].as_object_mut() {
                content.insert("contribution_receipt".into(), receipts[0].clone());
                if receipts.len() > 1 {
                    content.insert("contribution_receipts".into(), json!(receipts));
                }
            }
        }
        Ok(_) => {}
        Err(_) => {
            // Optional analytics never changes an operation's execution outcome.
            if let Some(content) = result["content"].as_array_mut() {
                content.push(json!({"type":"text","text":"AgentSCM contribution receipt unavailable; contribution coverage is unknown."}));
            }
        }
    }
}

/// Phase is a delivery identity, while version remains the shared metrics assessment identity.
/// A completion can be newly reported without changing any counted contribution evidence.
fn announcement_identity(receipt: &Value) -> String {
    format!(
        "{}:{}",
        receipt["phase"].as_str().unwrap_or("unknown"),
        receipt["version"].as_str().unwrap_or("unknown")
    )
}

fn response(receipt: Value) -> Value {
    json!({"content":[{"type":"text","text":receipt["text"]}],"structuredContent":receipt,"isError":false})
}

fn count(value: &Value, pointer: &str) -> String {
    match value.pointer(pointer) {
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::String(s)) if s.len() <= 40 && s.bytes().all(|b| b.is_ascii_digit()) => {
            s.clone()
        }
        _ => "unknown".into(),
    }
}

fn outcome_count(value: &Value, pointer: &str) -> String {
    if matches!(
        pointer,
        "/issues/dismissed"
            | "/issues/false_trigger"
            | "/followups/failed"
            | "/followups/cancelled"
    ) && value.pointer(pointer) == Some(&Value::Null)
    {
        "Not captured".into()
    } else {
        count(value, pointer)
    }
}

fn short(value: Option<&str>, limit: usize) -> String {
    let mut result = String::new();
    for ch in value
        .unwrap_or("unknown")
        .chars()
        .filter(|c| !c.is_control())
    {
        if result.len() + ch.len_utf8() > limit {
            break;
        }
        result.push(ch);
    }
    result
}

fn coverage_limits(projection: &Value) -> String {
    let gaps = projection["coverage"]["gaps"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut limits = vec!["partial capture"];
    for (needle, label) in [
        ("retention", "retention gap"),
        ("cap rejected", "capture cap"),
        ("capture failed", "capture failed"),
        (
            "provider account/endpoint namespace is unavailable",
            "provider namespace unknown",
        ),
        (
            "catch-up delivery records expire",
            "context retention limit",
        ),
        ("bounded source", "bounded source scan"),
    ] {
        if gaps
            .iter()
            .filter_map(Value::as_str)
            .any(|gap| gap.to_ascii_lowercase().contains(needle))
        {
            limits.push(label);
        }
    }
    limits.join(", ")
}

fn preview(
    _repository: &str,
    work_id: &str,
    generation: i64,
    phase: &str,
    projection: &Value,
) -> Value {
    // Only exact Work-linked provider evidence qualifies a merge update. Aggregate merge counts
    // and caller-authored "merged" checkpoints cannot establish this relationship.
    let evidence = projection["evidence"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let merge = projection
        .get("linked_merge")
        .filter(|e| {
            e["work_id"] == work_id && e["kind"] == "provider_merge" && e["state"] == "observed"
        })
        .or_else(|| {
            evidence.iter().find(|e| {
                e["work_id"] == work_id && e["kind"] == "provider_merge" && e["state"] == "observed"
            })
        });
    let phase = if merge.is_some() {
        "provider_observed_merge"
    } else {
        phase
    };
    let mut referenced = Vec::new();
    if let Some(merged) = merge {
        referenced.push(merged);
    }
    for fact in evidence {
        if referenced.len() == 3 {
            break;
        }
        if !referenced
            .iter()
            .any(|prior| prior["id"] == fact["id"] && prior["kind"] == fact["kind"])
        {
            referenced.push(fact);
        }
    }
    let references: Vec<Value> = referenced.iter().map(|e|json!({"id":short(e["id"].as_str(),128),"kind":short(e["kind"].as_str(),48),"state":short(e["state"].as_str(),48)})).collect();
    let rows = vec![
        format!(
            "AgentSCM contribution · {phase} · Work {} · source {}",
            short(Some(work_id), 64),
            short(projection["receipt_source"].as_str(), 64)
        ),
        format!(
            "Context: {} updates prepared for return; {} explicitly acknowledged; application not observed.",
            count(projection, "/context/returned"),
            count(projection, "/context/acknowledged")
        ),
        format!(
            "Issues ({}): {} unresolved; {} resolved; dismissed: {}; false triggers: {}.",
            short(projection["issues"]["availability"].as_str(), 24),
            count(projection, "/issues/unresolved"),
            count(projection, "/issues/resolved"),
            outcome_count(projection, "/issues/dismissed"),
            outcome_count(projection, "/issues/false_trigger")
        ),
        format!(
            "Follow-ups ({}): {} current; {} pending; failed: {}; {} stale; cancelled: {}; {} unknown.",
            short(projection["followups"]["availability"].as_str(), 24),
            count(projection, "/followups/current"),
            count(projection, "/followups/pending"),
            outcome_count(projection, "/followups/failed"),
            count(projection, "/followups/stale"),
            outcome_count(projection, "/followups/cancelled"),
            count(projection, "/followups/unknown")
        ),
        format!(
            "Usage: {} input / {} output recorded tokens; {} allocated requests missing cost; {} unallocated records; overhead unknown. Details: agentscm_value_metrics.",
            count(projection, "/resources/input_tokens"),
            count(projection, "/resources/output_tokens"),
            count(projection, "/resources/missing_cost_requests"),
            count(projection, "/resources/unallocated_usage_records")
        ),
        format!(
            "Coverage: {}; {}; {} omitted; window {}–{}. {}. Refs: {}. Details: agentscm_value_metrics(work_id).",
            short(projection["coverage"]["state"].as_str(), 32),
            coverage_limits(projection),
            count(projection, "/coverage/omitted"),
            count(projection, "/since"),
            count(projection, "/until"),
            if merge.is_some() {
                "provider merge observed; Work association declared; deployment unknown"
            } else if projection["merge_observations"].as_u64().unwrap_or(0) > 0 {
                "merge evidence outside preview; inspect details"
            } else {
                "merge not observed"
            },
            references
                .iter()
                .filter_map(|e| e["id"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ];
    let version = short(projection["assessment_id"].as_str(), 128);
    let gap_values = projection["coverage"]["gaps"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let gaps: Vec<String> = gap_values
        .iter()
        .take(8)
        .map(|v| short(v.as_str(), 160))
        .collect();
    let merge_source = merge.map(|e| short(e["source"].as_str(), 160));
    let merge_observation = merge.map(|e|json!({"id":short(e["id"].as_str(),128),"source":short(e["source"].as_str(),160),"basis":short(e["basis"].as_str(),160),"assurance":short(e["assurance"].as_str(),48),"applicability":short(e["applicability"].as_str(),64),"detail":short(e["detail"].as_str(),384)}));
    json!({"receipt_id":format!("contribution-{}",identity(&["contribution.v1",work_id])),"version":version,"work_id":work_id,"generation":generation,"phase":phase,"method":"observed_contribution_v1","rows":rows,"text":rows.join("\n"),"evidence":references,"merge_source":merge_source,"merge_observation":merge_observation,"coverage_complete":projection["coverage"]["complete"]==true,"window":{"since":projection["since"],"until":projection["until"]},"coverage_gaps":gaps,"additional_coverage_gaps":gap_values.len().saturating_sub(8),"details":{"tool":"agentscm_value_metrics","arguments":{"work_id":work_id,"limit":20}}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compact_preview_preserves_unresolved_stale_and_uncaptured_outcomes() {
        let p = json!({"assessment_id":"v1","context":{"returned":4,"acknowledged":1},"issues":{"unresolved":2,"resolved":1,"dismissed":null,"false_trigger":null},"followups":{"failed":null,"cancelled":null,"stale":2},"coverage":{"state":"partial","complete":false},"merge_observations":12,"evidence":(0..20).map(|i|json!({"id":format!("{i}-{}","x".repeat(900)),"kind":"stale_evidence","state":"unresolved","work_id":format!("work-{i}")})).collect::<Vec<_>>()});
        let receipt = preview("acme/widgets", "work-1", 1, "completion", &p);
        assert_eq!(receipt["rows"].as_array().unwrap().len(), 6);
        assert_eq!(receipt["evidence"].as_array().unwrap().len(), 3);
        assert!(receipt["text"].as_str().unwrap().len() < 2048);
        assert!(receipt.to_string().len() < 8192);
        assert!(receipt["text"].as_str().unwrap().contains("2 unresolved"));
        assert!(
            receipt["text"]
                .as_str()
                .unwrap()
                .contains("failed: Not captured")
        );
        assert!(receipt["text"].as_str().unwrap().contains("2 stale"));
        assert_eq!(receipt["phase"], "completion");
    }
    #[test]
    fn merge_advances_same_receipt_only_for_exact_work_provider_observation() {
        let mut p = json!({"assessment_id":"v1","evidence":[]});
        let before = preview("acme/widgets", "work-1", 1, "completion", &p);
        p["assessment_id"] = json!("v2");
        p["evidence"] = json!([{"id":"merge-1","kind":"provider_merge","state":"observed","work_id":"work-2","source":"forge:2@abc"}]);
        assert_eq!(
            preview("acme/widgets", "work-1", 1, "current", &p)["phase"],
            "current"
        );
        p["evidence"][0]["work_id"] = json!("work-1");
        let after = preview("acme/widgets", "work-1", 1, "current", &p);
        assert_eq!(before["receipt_id"], after["receipt_id"]);
        assert_ne!(before["version"], after["version"]);
        assert_eq!(after["phase"], "provider_observed_merge");
        assert_eq!(after["merge_source"], "forge:2@abc");
    }
    #[test]
    fn linked_merge_is_present_when_evidence_preview_page_omits_it() {
        let p = json!({"assessment_id":"merged-v2","evidence":[],"linked_merge":{"id":"provider-9","kind":"provider_merge","state":"observed","work_id":"work-1","source":"a".repeat(40)},"receipt_source":"a".repeat(40),"coverage":{"state":"partial","complete":false,"gaps":["retention gap"]},"since":123,"until":456});
        let receipt = preview("acme/widgets", "work-1", 1, "current", &p);
        assert_eq!(receipt["phase"], "provider_observed_merge");
        assert_eq!(receipt["merge_source"], "a".repeat(40));
        assert!(receipt["text"].as_str().unwrap().contains("window 123–456"));
        assert!(receipt["text"].as_str().unwrap().contains("source aaaaa"));
    }
    #[test]
    fn namespace_uncertainty_does_not_claim_a_known_source_is_unavailable() {
        let p = json!({"assessment_id":"v1","receipt_source":"a".repeat(40),"evidence":[],"coverage":{"state":"partial","gaps":["Provider account/endpoint namespace is unavailable; equal provider/request IDs are conservatively collapsed"]}});
        let receipt = preview("acme/widgets", "work-1", 1, "completion", &p);
        let text = receipt["text"].as_str().unwrap();
        assert!(text.contains("source aaaaa"));
        assert!(text.contains("provider namespace unknown"));
        assert!(!text.contains("source unavailable"));
    }
    #[test]
    fn announcement_phases_do_not_change_the_shared_assessment_version() {
        let check = json!({"phase":"check","version":"same-assessment"});
        let premerge = json!({"phase":"pre_merge","version":"same-assessment"});
        let completion = json!({"phase":"completion","version":"same-assessment"});
        let merged = json!({"phase":"provider_observed_merge","version":"same-assessment"});
        let identities = std::collections::BTreeSet::from([
            announcement_identity(&check),
            announcement_identity(&premerge),
            announcement_identity(&completion),
            announcement_identity(&merged),
        ]);
        assert_eq!(identities.len(), 4);
        assert_eq!(
            announcement_identity(&completion),
            "completion:same-assessment"
        );
    }
    #[test]
    fn only_uncaptured_outcome_fields_use_not_captured_for_null() {
        let p = json!({"issues":{"dismissed":null,"false_trigger":null,"unresolved":0},"followups":{"failed":null,"cancelled":null,"current":0},"context":{"applied":null},"resources":{"output_tokens":null}});
        for pointer in [
            "/issues/dismissed",
            "/issues/false_trigger",
            "/followups/failed",
            "/followups/cancelled",
        ] {
            assert_eq!(outcome_count(&p, pointer), "Not captured");
        }
        assert_eq!(outcome_count(&p, "/issues/unresolved"), "0");
        assert_eq!(outcome_count(&p, "/followups/current"), "0");
        assert_eq!(outcome_count(&p, "/context/applied"), "unknown");
        assert_eq!(outcome_count(&p, "/resources/output_tokens"), "unknown");
    }
}
