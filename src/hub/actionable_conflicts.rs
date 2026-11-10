//! Bounded, repository-scoped conflict facts for the shared status projection.
use super::{RecipientKind, StatusAction, StatusRecipient};
use crate::hub::intent::{Intent, key_of, overlaps};
use crate::hub::overlap_detail;
use crate::hub::session::{HOLD_CAP_SECONDS, same_agent};
use crate::hub::store::{Principal, Store};
use crate::hub::work::WorkItem;
use crate::hub::work_links::WorkPrLinkTarget;
use crate::hub::work_transition::WorkLifecycleReader;
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
/// The agents on one contested file: whom the row names, and their intents by id.
#[derive(Default)]
struct Contested<'a> {
    owners: BTreeMap<String, StatusRecipient>,
    holders: BTreeMap<&'a str, &'a Intent>,
}
impl<'a> Contested<'a> {
    fn add(&mut self, intent: &'a Intent) {
        self.owners
            .insert(intent.owner.id.clone(), recipient(intent));
        self.holders.insert(&intent.id, intent);
    }
}

/// Whether a holder's `specs` overlap any of a pull request's `files`, and whether the scan was
/// cut short. Every overlapping file found goes into `overlapping`. Finding the first one
/// spends `comparisons`; listing the rest spends `listing`, so a spent listing budget never
/// stops a later holder from being found.
fn held_files<'a>(
    files: impl Iterator<Item = &'a str>,
    specs: &[String],
    comparisons: &mut usize,
    listing: &mut usize,
    overlapping: &mut BTreeSet<&'a str>,
) -> (bool, bool) {
    let mut hit = false;
    for file in files {
        for spec in specs {
            let spent = if hit {
                &mut *listing
            } else {
                &mut *comparisons
            };
            *spent += 1;
            if *spent > COMPARISONS {
                return (hit, true);
            }
            if overlaps(key_of(file), key_of(spec)) {
                hit = true;
                overlapping.insert(file);
                break;
            }
        }
    }
    (hit, false)
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
pub(super) fn capture(hash: &mut Sha256, record: &[u8]) {
    hash.update((record.len() as u64).to_be_bytes());
    hash.update(record);
}

/// Whether the intent's own Work opened this pull request: the PR's head branch is the
/// intent's or its Work's branch, or the Work links this PR. Hooks and MCP may hold one
/// native session under two tokens, so the Work is found by owner and native session.
#[allow(clippy::too_many_arguments)]
fn opened_by(
    store: &dyn Store,
    reader: &WorkLifecycleReader<'_>,
    items: &BTreeMap<String, WorkItem>,
    targets: &mut BTreeMap<String, Option<WorkPrLinkTarget>>,
    intent: &Intent,
    number: i64,
    head: Option<&str>,
    now: i64,
) -> Result<bool> {
    if head.is_some() && intent.branch.as_deref() == head {
        return Ok(true);
    }
    let Some(session) = intent.session.as_deref() else {
        return Ok(false);
    };
    for item in items.values().filter(|i| {
        i.participants
            .iter()
            .any(|p| p.owner == intent.owner.id && p.session == session)
    }) {
        if head.is_some() && item.branch.as_deref() == head {
            return Ok(true);
        }
        let target = match targets.get(&item.id) {
            Some(target) => target.clone(),
            None => {
                let target = store.work_pr_target_scoped(reader, &item.id, now)?;
                targets.insert(item.id.clone(), target.clone());
                target
            }
        };
        if target.is_some_and(|t| t.number == number) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn read(
    store: &dyn Store,
    who: &Principal,
    reader: &WorkLifecycleReader<'_>,
    items: &BTreeMap<String, WorkItem>,
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
    let mut found = BTreeMap::<String, Contested<'_>>::new();
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
                        let contested = found.entry(path.clone()).or_default();
                        contested.add(a);
                        contested.add(b);
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
            let mut current = Vec::new();
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
                        current.push(i);
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
            let contested = found.entry(path.into()).or_default();
            for i in current {
                contested.add(i);
            }
            if let Some(i) = intents.iter().find(|i| i.token == event.token) {
                contested.add(i);
            } else if own_tokens
                .iter()
                .any(|t| t.id == event.token && !t.revoked && t.expires_at > now)
            {
                contested.owners.insert(
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
    for (path, contested) in found {
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
        let answering = pending && !repeated;
        let holders: Vec<&Intent> = contested.holders.into_values().collect();
        let mut row = action(
            format!("file:{path}"),
            "file_conflict",
            contested.owners.into_values().collect(),
            overlap_detail::file_conflict(&path, &holders, answering, now),
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
        if answering {
            row.needs_you = false;
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
        let mut targets = BTreeMap::new();
        let mut listing = 0;
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
            let mut holders = Vec::new();
            let mut overlapping = BTreeSet::new();
            // Finding a holder stops at its first overlapping file, as before. Listing its other
            // overlapping files for the text has a budget of its own, so the detail never costs
            // a later holder or pull request its check. A budget that ran out, or a file list
            // the forge cut short, leaves the count of the rest unknown.
            let mut rest_unknown = pull["files_truncated"] != false;
            for intent in &intents {
                // The agent that opened the pull request holding its own files is no overlap.
                if opened_by(
                    store,
                    reader,
                    items,
                    &mut targets,
                    intent,
                    number,
                    pull["head_ref"].as_str(),
                    now,
                )? {
                    continue;
                }
                let files = files
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|p| !p.chars().any(char::is_control));
                let (hit, cut) = held_files(
                    files,
                    &intent.paths,
                    &mut comparisons,
                    &mut listing,
                    &mut overlapping,
                );
                rest_unknown |= cut;
                if comparisons > COMPARISONS {
                    limits.insert("conflict_comparison_limit".into());
                }
                if hit {
                    recipients.insert(intent.owner.id.clone(), recipient(intent));
                    holders.push(intent);
                }
            }
            if !recipients.is_empty() {
                // By intent id, as a contested file lists its holders.
                holders.sort_by(|a, b| a.id.cmp(&b.id));
                let overlapping: Vec<&str> = overlapping.into_iter().collect();
                let mut row = action(
                    format!("pr:{}:{number}", repo.id),
                    "pr_overlap",
                    recipients.into_values().collect(),
                    overlap_detail::pull_overlap(
                        number,
                        &pull,
                        &overlapping,
                        rest_unknown,
                        &holders,
                        now,
                    ),
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
    fn a_spent_listing_budget_still_finds_the_next_holder() {
        let specs = ["src/a.rs".to_owned()];
        let files = ["src/a.rs", "src/b.rs", "src/c.rs"];
        let (mut comparisons, mut listing) = (0, COMPARISONS);
        let mut overlapping = BTreeSet::new();
        // The holder is found with the check budget; listing its other files is cut short.
        assert_eq!(
            held_files(
                files.into_iter(),
                &specs,
                &mut comparisons,
                &mut listing,
                &mut overlapping
            ),
            (true, true)
        );
        assert_eq!(comparisons, 1);
        // The next holder is still found, though the listing budget is spent.
        let next = ["src/c.rs".to_owned()];
        assert_eq!(
            held_files(
                files.into_iter(),
                &next,
                &mut comparisons,
                &mut listing,
                &mut overlapping
            ),
            (true, false)
        );
        assert_eq!(overlapping, BTreeSet::from(["src/a.rs", "src/c.rs"]));
        // A spent check budget finds nobody and says the scan was cut.
        comparisons = COMPARISONS;
        assert_eq!(
            held_files(
                files.into_iter(),
                &next,
                &mut comparisons,
                &mut listing,
                &mut overlapping
            ),
            (false, true)
        );
    }

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
