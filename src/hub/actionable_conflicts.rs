//! Bounded, repository-scoped conflict facts for the shared status projection.
use super::{RecipientKind, StatusAction, StatusRecipient};
use crate::hub::intent::{Intent, key_of, overlaps};
use crate::hub::session::{HOLD_CAP_SECONDS, same_agent};
use crate::hub::store::{Principal, Store};
use crate::hub::{HubError, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

const LIMIT: usize = 100;
const COMPARISONS: usize = 20_000;

fn recipient(intent: &Intent) -> StatusRecipient {
    StatusRecipient {
        kind: RecipientKind::Person,
        owner: Some(intent.owner.id.clone()),
        login: Some(intent.owner.login.clone()),
        agent: Some(intent.agent.clone()),
    }
}
fn paths(data: &Value) -> Vec<&str> {
    data["paths"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .take(8)
        .collect()
}
fn action(
    id: String,
    kind: &str,
    recipients: Vec<StatusRecipient>,
    text: String,
    seq: i64,
    viewer: &str,
) -> StatusAction {
    StatusAction {
        id,
        kind: kind.into(),
        work_ids: vec![],
        needs_you: recipients
            .iter()
            .any(|r| r.owner.as_deref() == Some(viewer)),
        recipients,
        action: text,
        updated_seq: seq,
        semantic_version: None,
        limits: vec![],
    }
}

/// Hash every captured byte independently of display and comparison caps. Collection
/// bounds limit the records; hashing adds no retained copy or separate evidence-size cap.
fn capture(hash: &mut Sha256, record: &[u8]) {
    hash.update((record.len() as u64).to_be_bytes());
    hash.update(record);
}

pub(super) fn read(
    store: &dyn Store,
    who: &Principal,
    now: i64,
) -> Result<(Vec<StatusAction>, BTreeSet<String>, String)> {
    let mut limits = BTreeSet::new();
    let mut evidence = Sha256::new();
    capture(
        &mut evidence,
        format!(
            "{}:{}:{}:{:?}",
            who.owner.identity.provider, who.owner.identity.host, who.repository, who.repository_id
        )
        .as_bytes(),
    );
    let mut intents = store.live_intents_scoped(who, now, LIMIT + 1)?;
    for intent in &intents {
        capture(&mut evidence, intent.to_json().to_string().as_bytes());
    }
    if intents.len() > LIMIT {
        limits.insert("live_intents_truncated".into());
        intents.truncate(LIMIT);
    }
    let events = store.repository_events(who, None, LIMIT)?;
    for event in &events {
        capture(
            &mut evidence,
            format!(
                "{}:{}:{}:{}:{}",
                event.event.seq, event.event.kind, event.token, event.event.at, event.event.data
            )
            .as_bytes(),
        );
    }
    if events.len() == LIMIT {
        limits.insert("conflict_history_truncated".into());
    }
    let parsed = events
        .iter()
        .map(|e| serde_json::from_str::<Value>(&e.event.data).map(|v| (e, v)))
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| HubError::Unavailable("conflict event decoding failed".into()))?;
    if parsed
        .iter()
        .any(|(_, data)| data["paths"].as_array().is_some_and(|p| p.len() > 8))
    {
        limits.insert("conflict_event_paths_truncated".into());
    }
    let mut found = BTreeMap::<String, BTreeMap<String, StatusRecipient>>::new();
    let mut comparisons = 0;
    'pairs: for (n, a) in intents.iter().enumerate() {
        for b in &intents[n + 1..] {
            if same_agent(&a.token, &a.via, b) {
                continue;
            }
            for x in &a.paths {
                for y in &b.paths {
                    comparisons += 1;
                    if comparisons > COMPARISONS {
                        limits.insert("conflict_comparison_limit".into());
                        break 'pairs;
                    }
                    if overlaps(key_of(x), key_of(y)) {
                        let path = if key_of(x).len() >= key_of(y).len() {
                            x
                        } else {
                            y
                        };
                        if found.len() >= 200 && !found.contains_key(path) {
                            limits.insert("file_conflicts_truncated".into());
                            continue;
                        }
                        let owners = found.entry(path.clone()).or_default();
                        owners.insert(a.owner.id.clone(), recipient(a));
                        owners.insert(b.owner.id.clone(), recipient(b));
                    }
                }
            }
        }
    }
    // Historical refusals are current only while the same requester session and a named
    // holder from that refusal remain live. Token validity alone cannot prove contention.
    let sessions = store.live_sessions(who, now, LIMIT + 1)?;
    for session in &sessions {
        capture(
            &mut evidence,
            format!(
                "{}:{}:{}:{:?}:{}:{}",
                session.key,
                session.token,
                session.via,
                session.intent,
                session.created_at,
                session.updated_at
            )
            .as_bytes(),
        );
    }
    if sessions.len() > LIMIT {
        limits.insert("conflict_sessions_truncated".into());
    }
    let own_tokens = store.list_agent_tokens(&who.owner.id)?;
    let mut unknown_recipients = BTreeSet::new();
    let mut live_repeated = BTreeSet::new();
    'event_paths: for (event, data) in &parsed {
        if event.event.kind != "edit_repeated" {
            continue;
        }
        let Some(session_id) = data["session"].as_str() else {
            limits.insert("conflict_session_unknown".into());
            continue;
        };
        let key = crate::hub::store::session_key(&event.token, session_id);
        if !sessions
            .iter()
            .take(LIMIT)
            .any(|s| s.key == key && s.created_at <= event.event.at)
            || parsed.iter().any(|(e, d)| {
                e.token == event.token
                    && e.event.seq > event.event.seq
                    && matches!(e.event.kind.as_str(), "session_ended" | "session_started")
                    && d["session"] == session_id
            })
        {
            continue;
        }
        for path in paths(data) {
            let Some((_, denied)) = parsed.iter().find(|(e, d)| {
                e.event.kind == "edit_denied"
                    && e.token == event.token
                    && e.event.seq < event.event.seq
                    && d["session"] == session_id
                    && paths(d).iter().any(|p| overlaps(key_of(p), key_of(path)))
            }) else {
                continue;
            };
            let holders: BTreeSet<&str> = denied["holders"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let mut current = BTreeMap::new();
            for i in &intents {
                if !holders.contains(i.id.as_str()) {
                    continue;
                }
                for spec in &i.paths {
                    comparisons += 1;
                    if comparisons > COMPARISONS {
                        limits.insert("conflict_comparison_limit".into());
                        break 'event_paths;
                    }
                    if overlaps(key_of(spec), key_of(path)) {
                        current.insert(i.owner.id.clone(), recipient(i));
                        break;
                    }
                }
            }
            if current.is_empty() {
                continue;
            }
            if found.len() >= 200 && !found.contains_key(path) {
                limits.insert("file_conflicts_truncated".into());
                continue;
            }
            live_repeated.insert(path.to_owned());
            let owners = found.entry(path.into()).or_default();
            owners.extend(current);
            if let Some(i) = intents.iter().find(|i| i.token == event.token) {
                owners.insert(i.owner.id.clone(), recipient(i));
            } else if own_tokens
                .iter()
                .any(|t| t.id == event.token && !t.revoked && t.expires_at > now)
            {
                owners.insert(
                    who.owner.id.clone(),
                    StatusRecipient {
                        kind: RecipientKind::Person,
                        owner: Some(who.owner.id.clone()),
                        login: Some(who.owner.login.clone()),
                        agent: Some(event.agent.clone()),
                    },
                );
            } else {
                unknown_recipients.insert(path.to_owned());
            }
        }
    }
    let mut actions = Vec::new();
    for (path, owners) in found {
        let handoff = parsed.iter().find(|(e, d)| {
            e.event.kind == "handoff_request"
                && paths(d).iter().any(|p| overlaps(key_of(p), key_of(&path)))
        });
        let pending = handoff.is_some_and(|(request, d)| {
            let id = d["request"].as_str();
            id.is_some()
                && now.saturating_sub(request.event.at) <= HOLD_CAP_SECONDS
                && !parsed.iter().any(|(e, answer)| {
                    matches!(e.event.kind.as_str(), "handoff_grant" | "handoff_decline")
                        && answer["request"].as_str() == id
                })
        });
        let repeated = live_repeated.contains(&path);
        let mut row = action(
            format!("file:{path}"),
            "file_conflict",
            owners.into_values().collect(),
            format!("Agree who keeps the overlapping file {path}."),
            parsed
                .iter()
                .filter(|(_, d)| paths(d).iter().any(|p| overlaps(key_of(p), key_of(&path))))
                .map(|(e, _)| e.event.seq)
                .max()
                .unwrap_or(0),
            &who.owner.id,
        );
        if row.recipients.is_empty() || unknown_recipients.contains(&path) {
            row.limits.push("conflict_recipient_unknown".into());
            limits.insert("conflict_recipient_unknown".into());
        }
        if pending && !repeated {
            row.needs_you = false;
            row.action = format!("The agents are answering the pending handoff for {path}.");
        }
        actions.push(row);
    }
    let repo = match who.repository_id {
        Some(id) => store.repository_by_id(&who.owner.identity.host, id)?,
        None => store.repository_named(&who.owner.identity.host, &who.repository)?,
    };
    if let Some(repo) = repo {
        let connection = store.connection(&repo.connection)?;
        if connection.as_ref().is_none_or(|c| {
            c.provider != who.owner.identity.provider || c.host != who.owner.identity.host
        }) {
            return Err(HubError::Unauthorized);
        }
        let pulls = store.observations(&repo.id, "pull_request")?;
        for pull in &pulls {
            capture(
                &mut evidence,
                format!(
                    "{}:{}:{}:{}",
                    repo.id, pull.key, pull.observed_at, pull.data
                )
                .as_bytes(),
            );
        }
        if pulls.len() >= 1000 {
            limits.insert("pr_observations_truncated".into());
        }
        for observed in pulls.iter().take(LIMIT) {
            if comparisons >= COMPARISONS {
                limits.insert("conflict_comparison_limit".into());
                break;
            }
            let pull: Value = serde_json::from_str(&observed.data)
                .map_err(|_| HubError::Unavailable("PR conflict decoding failed".into()))?;
            if pull["files_truncated"] == true {
                limits.insert("pr_paths_truncated".into());
            }
            if pull["state"] != "open" {
                continue;
            }
            let Some(number) = pull["number"].as_i64().filter(|n| *n > 0) else {
                limits.insert("pr_identity_unknown".into());
                continue;
            };
            if observed.key != number.to_string() {
                limits.insert("pr_identity_unknown".into());
                continue;
            }
            let Some(files) = pull["files"].as_array() else {
                limits.insert("pr_paths_unknown".into());
                continue;
            };
            let mut recipients = BTreeMap::new();
            for intent in &intents {
                let mut hit = false;
                for file in files
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|p| !p.chars().any(char::is_control))
                {
                    for spec in &intent.paths {
                        comparisons += 1;
                        if comparisons > COMPARISONS {
                            limits.insert("conflict_comparison_limit".into());
                            break;
                        }
                        if overlaps(key_of(file), key_of(spec)) {
                            hit = true;
                            break;
                        }
                    }
                    if hit || comparisons > COMPARISONS {
                        break;
                    }
                }
                if hit {
                    recipients.insert(intent.owner.id.clone(), recipient(intent));
                }
            }
            if !recipients.is_empty() {
                let mut row = action(
                    format!("pr:{}:{number}", repo.id),
                    "pr_overlap",
                    recipients.into_values().collect(),
                    format!("Review pull request #{number} against the named holders' files."),
                    0, // Forge reads do not carry a monotone Work event sequence.
                    &who.owner.id,
                );
                let canonical_files: BTreeSet<&str> =
                    files.iter().filter_map(Value::as_str).collect();
                let version = serde_json::to_vec(&(
                    &row.id,
                    &pull["head_sha"],
                    canonical_files,
                    &pull["files_truncated"],
                    &row.recipients,
                ))
                .expect("PR action version serializes");
                row.semantic_version =
                    Some(crate::hub::store::lower_hex_of(&Sha256::digest(version)));
                actions.push(row);
            }
        }
        if pulls.len() > LIMIT {
            limits.insert("pr_observations_truncated".into());
        }
    } else {
        limits.insert("pr_binding_unavailable".into());
    }
    Ok((
        actions,
        limits,
        crate::hub::store::lower_hex_of(&evidence.finalize()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_length_evidence_changes_beyond_sixteen_mib_change_the_digest() {
        let prefix = vec![b'x'; 16 * 1024 * 1024];
        let digest = |record: &[u8]| {
            let mut hash = Sha256::new();
            capture(&mut hash, &prefix);
            capture(&mut hash, record);
            hash.finalize()
        };
        assert_ne!(
            digest(br#"{"head_sha":"first"}"#),
            digest(br#"{"head_sha":"other"}"#)
        );
    }
}
