//! Failed required checks and stopped skill gates on pull requests and Work, for the shared
//! status projection. Provider facts come only from the retained snapshot of an open pull
//! request's exact head and base; nothing here calls the forge or reads agent prose.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::conflicts::capture;
use super::{ActionableStatus, RecipientKind, StatusAction, StatusRecipient, StatusWork};
use crate::hub::advice::sanitize;
use crate::hub::store::{ForgeIdentity, Principal, Store, lower_hex_of, session_key};
use crate::hub::work::{CheckpointKind, ClaimStatus, WorkItem};
use crate::hub::{HubError, Result};
use crate::skill_workflow::supported::{self, RunState};

/// The signed local-validation attestation status.
const LOCAL_VALIDATION: &str = "agentscm/local-validation";
/// The Hub's own forge checks; see `forge_check::status_context`.
const HUB_CHECK_PREFIX: &str = "agentscm/workflow/";
const PULLS: usize = 100;
const RUNS: usize = 100;
const SESSIONS: usize = 100;

/// The required contexts, in the Hub's own spelling, so ids and text stay plain: the legacy
/// local-validation context, `agentscm/local-validation/<id>` and
/// `agentscm/local-validation/<id>/<constituent>` (ids of `[a-z0-9-]`), and the Hub's own checks.
/// Anything else, including look-alikes such as `agentscm/local-validation-foo`, is not reported.
fn required(name: &str) -> bool {
    let id = |s: &str| {
        !s.is_empty()
            && s.len() <= 40
            && s.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    name == LOCAL_VALIDATION
        || name
            .strip_prefix(LOCAL_VALIDATION)
            .and_then(|rest| rest.strip_prefix('/'))
            .is_some_and(|rest| {
                let mut parts = rest.split('/');
                let (check, constituent, extra) = (parts.next(), parts.next(), parts.next());
                extra.is_none() && check.is_some_and(id) && constituent.is_none_or(id)
            })
        || name.strip_prefix(HUB_CHECK_PREFIX).is_some_and(|target| {
            crate::hub::forge_check::status_context(target).is_ok_and(|c| c == name)
        })
}

fn person(owner: &str, login: &str) -> StatusRecipient {
    StatusRecipient {
        kind: RecipientKind::Person,
        owner: Some(owner.to_owned()),
        login: Some(login.to_owned()),
        agent: None,
    }
}

fn version(parts: &impl serde::Serialize) -> String {
    lower_hex_of(&Sha256::digest(
        serde_json::to_vec(parts).expect("check version serializes"),
    ))
}

/// Whose a pull request or branch is: the forge author when that person uses this Hub, the
/// owner of the Hub publication that opened it, and the owners of unfinished Work on its branch.
struct People<'a> {
    store: &'a dyn Store,
    who: &'a Principal,
    items: &'a BTreeMap<String, WorkItem>,
    /// By pull request number: the forge URL the Hub opened it at, and the publication owner.
    publications: BTreeMap<i64, (Option<String>, StatusRecipient)>,
}

impl People<'_> {
    fn of(
        &self,
        pr: Option<&Value>,
        branch: Option<&str>,
    ) -> Result<(BTreeMap<String, StatusRecipient>, Vec<String>)> {
        let mut people = BTreeMap::new();
        if let Some(pr) = pr {
            if let Some(user_id) = pr["author"]["id"].as_i64() {
                let identity = ForgeIdentity {
                    provider: self.who.owner.identity.provider.clone(),
                    host: self.who.owner.identity.host.clone(),
                    user_id,
                };
                if let Some(user) = self.store.user_for_identity(&identity)?
                    && !user.disabled
                {
                    people.insert(user.id.clone(), person(&user.id, &user.login));
                }
            }
            // Publications are keyed by repository name alone, so only the exact pull request
            // URL proves one opened this pull request rather than a same-named one elsewhere.
            if let Some((_, owner)) = pr["number"]
                .as_i64()
                .and_then(|n| self.publications.get(&n))
                .filter(|(url, _)| url.is_some() && url.as_deref() == pr["html_url"].as_str())
            {
                people.insert(owner.owner.clone().unwrap_or_default(), owner.clone());
            }
        }
        let branch = branch.or_else(|| pr.and_then(|pr| pr["head_ref"].as_str()));
        let mut work_ids = Vec::new();
        for item in self
            .items
            .values()
            .filter(|i| branch.is_some() && i.branch.as_deref() == branch)
        {
            people.insert(item.owner.clone(), person(&item.owner, &item.owner_login));
            work_ids.push(item.id.clone());
        }
        Ok((people, work_ids))
    }
}

fn action(
    id: String,
    kind: &str,
    (people, work_ids): (BTreeMap<String, StatusRecipient>, Vec<String>),
    text: String,
    semantic_version: String,
    viewer: &str,
) -> StatusAction {
    let limits = if people.is_empty() {
        vec!["check_recipient_unknown".to_owned()]
    } else {
        vec![]
    };
    StatusAction {
        id,
        kind: kind.into(),
        work_ids,
        needs_you: people.contains_key(viewer),
        recipients: people.into_values().collect(),
        action: text,
        // Forge reads and skill evidence carry no monotone Work event sequence.
        updated_seq: 0,
        semantic_version: Some(semantic_version),
        limits,
    }
}

/// While any of these is present, a missing check or gate action is unknown, not cleared.
const COVERAGE_LIMITS: &[&str] = &[
    "check_binding_unavailable",
    "check_evidence_unavailable",
    "check_evidence_incomplete",
    "check_pulls_truncated",
    "check_publications_truncated",
    "pr_identity_unknown",
    "pr_observations_truncated",
    "skill_runs_truncated",
    "skill_gate_sessions_truncated",
    "status_actions_truncated",
    "status_response_budget_truncated",
];

/// Whether a lead has stopped working its Work: its claim is quiet, or it paused or handed over.
fn stopped(work: &StatusWork) -> bool {
    work.freshness != ClaimStatus::Active
        || work
            .operational
            .report
            .as_ref()
            .is_some_and(|r| matches!(r.kind, CheckpointKind::Pause | CheckpointKind::Handover))
}

/// Slack's rule for these actions: only a failure that blocks a merge after its agent stopped.
/// Both kinds block a merge (required contexts, pre-merge gates). A gate row exists only once its
/// agent stopped; a failed check needs linked Work whose every lead stopped, else it stays unknown.
pub fn merge_blocked_after_stop(status: &ActionableStatus, action: &StatusAction) -> bool {
    match action.kind.as_str() {
        "skill_gate" => true,
        "failed_check" => {
            !action.work_ids.is_empty()
                && action.work_ids.iter().all(|id| {
                    status
                        .work
                        .iter()
                        .find(|w| &w.work_id == id)
                        .is_some_and(stopped)
                })
        }
        _ => false,
    }
}

/// A check or gate action is confirmed gone only from a read that covered every check source.
pub fn check_cleared(status: &ActionableStatus, id: &str) -> bool {
    !status
        .limits
        .iter()
        .any(|l| COVERAGE_LIMITS.contains(&l.as_str()))
        && !status.actions.iter().any(|a| a.id == id)
}

/// `items` are the Work rows the caller read consistently; Work on a check's branch links to it.
pub(super) fn read(
    store: &dyn Store,
    who: &Principal,
    items: &BTreeMap<String, WorkItem>,
    now: i64,
) -> Result<(Vec<StatusAction>, BTreeSet<String>, String)> {
    let mut limits = BTreeSet::new();
    let mut evidence = Sha256::new();
    let mut actions = Vec::new();
    let viewer = who.owner.id.as_str();
    let repo = match who.repository_id {
        Some(id) => store.repository_by_id(&who.owner.identity.host, id)?,
        None => store.repository_named(&who.owner.identity.host, &who.repository)?,
    };
    let publications = store.pull_publications(&who.repository, 0, PULLS)?;
    if publications.len() >= PULLS {
        limits.insert("check_publications_truncated".to_owned());
    }
    let mut people = People {
        store,
        who,
        items,
        publications: BTreeMap::new(),
    };
    for publication in &publications {
        capture(
            &mut evidence,
            format!(
                "{}:{:?}:{:?}:{}:{}",
                publication.id,
                publication.pr_number,
                publication.pr_url,
                publication.owner,
                publication.revision
            )
            .as_bytes(),
        );
        if let Some(number) = publication.pr_number {
            people.publications.insert(
                number,
                (
                    publication.pr_url.clone(),
                    person(&publication.owner, &publication.owner_login),
                ),
            );
        }
    }
    let mut open = BTreeMap::new();
    match &repo {
        Some(repo) => {
            let connection = store.connection(&repo.connection)?;
            if connection.as_ref().is_none_or(|c| {
                c.provider != who.owner.identity.provider || c.host != who.owner.identity.host
            }) {
                return Err(HubError::Unauthorized);
            }
            let enabled = connection.is_some_and(|c| c.status == "enabled");
            let pulls = store.observations(&repo.id, "pull_request")?;
            if pulls.len() >= 1000 {
                limits.insert("pr_observations_truncated".into());
            }
            // Only open pull requests' snapshots are read; closed ones may stay retained.
            let health = store.observation(&repo.id, "provider_context", "_health")?;
            for observed in &pulls {
                let pr: Value = serde_json::from_str(&observed.data)
                    .map_err(|_| HubError::Unavailable("PR check decoding failed".into()))?;
                if pr["state"] != "open" {
                    continue;
                }
                let Some(number) = pr["number"].as_i64().filter(|n| *n > 0) else {
                    limits.insert("pr_identity_unknown".into());
                    continue;
                };
                if open.len() >= PULLS {
                    limits.insert("check_pulls_truncated".into());
                    break;
                }
                open.insert(number, pr.clone());
                let rows = health
                    .iter()
                    .cloned()
                    .chain(store.observation(&repo.id, "provider_context", &number.to_string())?)
                    .collect::<Vec<_>>();
                for row in &rows {
                    capture(
                        &mut evidence,
                        format!("{}:{}:{}", row.key, row.observed_at, row.data).as_bytes(),
                    );
                }
                let context = json!({"pull_request": {"number": number,
                    "head_sha": pr["head_sha"], "base_sha": pr["base_sha"]}});
                let found = crate::hub::pr_context::provider_evidence(
                    &context,
                    repo.repository_id,
                    &rows,
                    enabled,
                );
                if found["availability"] != "observed" {
                    limits.insert("check_evidence_unavailable".into());
                    continue;
                }
                let statuses = &found["snapshot"]["commit_statuses"];
                if statuses["complete"] != true {
                    limits.insert("check_evidence_incomplete".into());
                }
                // The provider lists statuses newest first: the first row per context is current.
                let mut seen = BTreeSet::new();
                for status in statuses["items"].as_array().into_iter().flatten() {
                    let (Some(name), Some(state)) =
                        (status["name"].as_str(), status["state"].as_str())
                    else {
                        continue;
                    };
                    if !required(name)
                        || !seen.insert(name)
                        || status["head_relation"] != "same"
                        || !matches!(state, "failure" | "error")
                    {
                        continue;
                    }
                    let id = format!("check:{}:{number}:{name}", repo.id);
                    let semantic = version(&(
                        &id,
                        &found["snapshot"]["head_sha"],
                        state,
                        &status["provider_updated_at"],
                    ));
                    actions.push(action(
                        id,
                        "failed_check",
                        people.of(Some(&pr), None)?,
                        format!(
                            "Required check {name} reports {state} on pull request #{number}; fix or rerun it before merging."
                        ),
                        semantic,
                        viewer,
                    ));
                }
            }
        }
        None => {
            limits.insert("check_binding_unavailable".into());
        }
    }

    // A pr-gate run whose Stop hook blocks at a gate its agent must act on, once no session
    // that recorded its evidence is live: nobody is working through the gate any more. Only
    // runs on an open pull request or unfinished Work count; older branches are history.
    let views = crate::hub::skills::views(store, &who.repository, None, None, RUNS)?;
    if views.len() >= RUNS {
        limits.insert("skill_runs_truncated".into());
    }
    let blocked = views
        .iter()
        .filter(|v| v.state == RunState::Open && supported::stop_blocks(v))
        .collect::<Vec<_>>();
    if !blocked.is_empty() {
        let sessions = store.live_sessions(who, now, SESSIONS + 1)?;
        let live = sessions
            .iter()
            .take(SESSIONS)
            .map(|s| s.key.as_str())
            .collect::<BTreeSet<_>>();
        let own = store
            .list_agent_tokens(viewer)?
            .into_iter()
            .map(|t| t.id)
            .collect::<BTreeSet<_>>();
        for view in blocked {
            let gate = view.current_gate.as_deref().unwrap_or_default();
            capture(
                &mut evidence,
                format!(
                    "{}:{}:{}:{}:{}",
                    view.run_id, gate, view.head_sha, view.updated_at, view.next_action
                )
                .as_bytes(),
            );
            let recorded = view
                .gates
                .iter()
                .flat_map(|g| &g.evidence)
                .chain(&view.stale_evidence)
                .collect::<Vec<_>>();
            if recorded
                .iter()
                .any(|e| live.contains(session_key(&e.token_id, &e.session).as_str()))
            {
                continue;
            }
            if sessions.len() > SESSIONS {
                limits.insert("skill_gate_sessions_truncated".into());
                continue;
            }
            let pr = view
                .pull_request
                .and_then(|n| i64::try_from(n).ok())
                .and_then(|n| open.get(&n));
            let (mut recipients, work_ids) = people.of(pr, Some(&view.branch))?;
            if pr.is_none() && work_ids.is_empty() {
                continue;
            }
            if recorded.iter().any(|e| own.contains(&e.token_id)) {
                recipients.insert(viewer.to_owned(), person(viewer, &who.owner.login));
            }
            let id = format!("gate:{}:{gate}", view.run_id);
            let semantic = version(&(&id, &view.head_sha, &view.next_action));
            actions.push(action(
                id,
                "skill_gate",
                (recipients, work_ids),
                format!(
                    "{} is at its {gate} gate on branch {} and the agent that recorded it has stopped: {}",
                    sanitize(&view.skill.name, 64),
                    sanitize(&view.branch, 160),
                    sanitize(&view.next_action, 400)
                ),
                semantic,
                viewer,
            ));
        }
    }
    if actions.iter().any(|a| a.recipients.is_empty()) {
        limits.insert("check_recipient_unknown".into());
    }
    Ok((actions, limits, lower_hex_of(&evidence.finalize())))
}

#[cfg(test)]
mod tests {
    use super::required;

    #[test]
    fn named_local_validation_contexts_are_required_and_look_alikes_are_not() {
        for yes in [
            "agentscm/local-validation",
            "agentscm/local-validation/static",
            "agentscm/local-validation/static/react-doctor",
            "agentscm/workflow/main",
        ] {
            assert!(required(yes), "{yes}");
        }
        for no in [
            "agentscm/local-validation-foo",
            "agentscm/local-validation/",
            "agentscm/local-validation//x",
            "agentscm/local-validation/Static",
            "agentscm/local-validation/a/b/c",
            "agentscm/local-validation/a b",
            "agentscm/local-validation/a/",
            "agentscm/workflow/",
            "ci/unrelated",
        ] {
            assert!(!required(no), "{no}");
        }
    }
}
