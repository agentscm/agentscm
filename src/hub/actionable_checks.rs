//! Failed required checks and stopped skill gates on pull requests and Work, for the shared
//! status projection. Provider facts come only from the retained snapshot of an open pull
//! request's exact head and base; nothing here calls the forge or reads agent prose.
use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::conflicts::capture;
use super::{ActionableStatus, RecipientKind, StatusAction, StatusRecipient, StatusWork};
use crate::hub::advice::sanitize;
use crate::hub::intent::{key_of, overlaps};
use crate::hub::store::{
    ForgeIdentity, Principal, Store, lower_hex_of, principal_scope, session_key,
};
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
            crate::hub::github::status_context(target).is_ok_and(|c| c == name)
        })
}

/// The legacy context and the Hub's own checks keep the "Required check" wording. Named
/// local-validation contexts are reported by name shape, and may be non-required while a
/// repository migrates, so their text makes no claim about merge requirements.
fn failed_check_text(name: &str, state: &str, number: impl std::fmt::Display) -> String {
    if name
        .strip_prefix(LOCAL_VALIDATION)
        .is_some_and(|rest| rest.starts_with('/'))
    {
        format!(
            "Local validation check {name} reports {state} on pull request #{number}; fix or rerun it."
        )
    } else {
        format!(
            "Required check {name} reports {state} on pull request #{number}; fix or rerun it before merging."
        )
    }
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
            // Only the exact pull request URL proves a publication opened this pull request.
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
        session: None,
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

/// How long another owner's open pull request must go without a push, review or comment before
/// it stops holding a ready merge on the same files. The interim merge rule in AGENTS.md uses
/// the same window: longer than a review and CI round trip, short enough to end a stall the
/// same working day.
pub const MERGE_HOLD_IDLE_SECONDS: i64 = 4 * 3600;

/// Whether every check on the snapshot's head passed: both lists read in full and not empty
/// together, every check run completed as success, neutral or skipped, and every status
/// context's newest row a success. The provider returns only the latest run per check name.
fn green(snapshot: &Value) -> bool {
    let (runs, statuses) = (&snapshot["check_runs"], &snapshot["commit_statuses"]);
    if runs["complete"] != true || statuses["complete"] != true {
        return false;
    }
    let runs = runs["items"].as_array().map_or(&[][..], Vec::as_slice);
    let mut seen = BTreeSet::new();
    let current: Vec<&Value> = statuses["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|s| s["name"].as_str().is_some_and(|n| seen.insert(n)))
        .collect();
    (!runs.is_empty() || !current.is_empty())
        && runs.iter().all(|r| {
            r["state"] == "completed"
                && matches!(
                    r["conclusion"].as_str(),
                    Some("success" | "neutral" | "skipped")
                )
        })
        && current
            .iter()
            .all(|s| s["state"] == "success" && s["head_relation"] == "same")
}

/// `YYYY-MM-DD HH:MM UTC`.
fn utc(at: i64) -> String {
    let (y, mo, d, h, mi, _) = crate::civil::utc_civil(at);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// Up to three names, then how many more.
fn listed(names: &[String]) -> String {
    let mut text = names.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > 3 {
        text.push_str(&format!(" and {} more", names.len() - 3));
    }
    text
}

/// Another owner's open pull request that changes files a ready one changes.
struct Blocker {
    number: i64,
    login: String,
    files: Vec<String>,
    /// Its last push, review or comment as the forge reports it; `None` when not observed.
    active_at: Option<i64>,
    /// Whether a live AgentSCM hold of its owner covers a shared file.
    held: bool,
}

impl Blocker {
    fn idle(&self, now: i64) -> bool {
        !self.held
            && self
                .active_at
                .is_some_and(|at| now.saturating_sub(at) >= MERGE_HOLD_IDLE_SECONDS)
    }

    fn describe(&self, now: i64) -> String {
        let state = if self.held {
            "its owner holds them in AgentSCM".to_owned()
        } else {
            match self.active_at {
                Some(at) if self.idle(now) => {
                    format!("no push, review or comment since {}", utc(at))
                }
                Some(at) => format!("last push, review or comment {}", utc(at)),
                None => "last activity unknown".to_owned(),
            }
        };
        format!(
            "#{} by @{} ({}; {state})",
            self.number,
            self.login,
            listed(&self.files)
        )
    }
}

/// One row per ready pull request of unfinished Work that another owner's open pull request
/// holds by changing the same files. When every such owner is idle (no live hold and no push,
/// review or comment for `MERGE_HOLD_IDLE_SECONDS`) and the read was complete, it asks the
/// waiting pull request's person to merge first and notify, or wait. Otherwise it is an overlap
/// for the waiting Work's agents to settle with the other owner's, and asks no person.
#[allow(clippy::too_many_arguments)]
fn held_merges(
    store: &dyn Store,
    who: &Principal,
    repo: &str,
    people: &People<'_>,
    open: &BTreeMap<i64, Value>,
    ready: &[i64],
    complete: bool,
    now: i64,
    limits: &mut BTreeSet<String>,
) -> Result<Vec<StatusAction>> {
    let mut intents = store.live_intents_scoped(who, now, PULLS + 1)?;
    // An unread hold could be the one that keeps an owner active.
    let complete = complete && intents.len() <= PULLS;
    if intents.len() > PULLS {
        limits.insert("merge_hold_intents_truncated".into());
        intents.truncate(PULLS);
    }
    // Each open pull request once: its files, and its owners as `People::of` finds them (the
    // author who uses this Hub, the owner of the publication that opened it, the Work on its
    // branch). Pull requests the Hub opened share one bot author, so the author alone is not who.
    let mut owned = BTreeMap::new();
    for (number, pr) in open {
        let (persons, work_ids) = people.of(Some(pr), None)?;
        let files: BTreeSet<String> = pr["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|p| !p.chars().any(char::is_control))
            .map(str::to_owned)
            .collect();
        owned.insert(*number, (persons, work_ids, files));
    }
    // A file list the forge cut short could hide a file an active owner shares.
    let complete = complete && open.values().all(|pr| pr["files_truncated"] == false);
    let mut actions = Vec::new();
    for number in ready {
        let waiting = &open[number];
        let (persons, work_ids, mine) = &owned[number];
        if work_ids.is_empty() {
            continue;
        }
        let mut blockers = Vec::new();
        for (other, pr) in open {
            let (theirs, _, files) = &owned[other];
            let same = if theirs.is_empty() {
                // Nobody on this Hub owns it: only its forge author says whose it is.
                let Some(author) = pr["author"]["id"].as_i64() else {
                    limits.insert("merge_hold_author_unknown".into());
                    continue;
                };
                waiting["author"]["id"].as_i64() == Some(author)
            } else {
                theirs.keys().any(|owner| persons.contains_key(owner))
            };
            if other == number || same {
                continue;
            }
            let shared: Vec<String> = files.intersection(mine).cloned().collect();
            if shared.is_empty() {
                continue;
            }
            let held = intents.iter().any(|i| {
                theirs.contains_key(&i.owner.id)
                    && i.paths
                        .iter()
                        .any(|spec| shared.iter().any(|f| overlaps(key_of(f), key_of(spec))))
            });
            blockers.push(Blocker {
                number: *other,
                login: sanitize(pr["author"]["login"].as_str().unwrap_or("unknown"), 40),
                files: shared.iter().map(|f| sanitize(f, 160)).collect(),
                active_at: pr["updated_at"]
                    .as_str()
                    .and_then(crate::hub::github::parse_rfc3339),
                held,
            });
        }
        if blockers.is_empty() {
            continue;
        }
        let idle = complete && blockers.iter().all(|b| b.idle(now));
        let named = listed(&blockers.iter().map(|b| b.describe(now)).collect::<Vec<_>>());
        let (theirs, owner) = if blockers.len() == 1 {
            ("another owner's open pull request changes", "its owner")
        } else {
            ("other owners' open pull requests change", "their owners")
        };
        let held = format!(
            "Pull request #{number} passed its checks, but {theirs} the same files: {named}."
        );
        let id = format!("held:{repo}:{number}");
        let semantic = version(&(
            &id,
            &waiting["head_sha"],
            blockers
                .iter()
                .map(|b| (b.number, &open[&b.number]["head_sha"], b.idle(now)))
                .collect::<Vec<_>>(),
            idle,
        ));
        if idle {
            actions.push(action(
                id,
                "merge_held",
                (persons.clone(), work_ids.clone()),
                format!(
                    "{held} None has had a push, review or comment for {} hours, and none is held in AgentSCM. Decide: merge #{number} first and leave one comment on each naming the shared files, or wait for {owner}.",
                    MERGE_HOLD_IDLE_SECONDS / 3600
                ),
                semantic,
                &who.owner.id,
            ));
        } else {
            let agents = people
                .items
                .values()
                .filter(|i| work_ids.contains(&i.id))
                .map(|i| {
                    (
                        i.owner.clone(),
                        StatusRecipient {
                            kind: RecipientKind::LeadAgent,
                            owner: Some(i.owner.clone()),
                            login: Some(i.owner_login.clone()),
                            agent: Some(i.lead_agent.clone()),
                        },
                    )
                })
                .collect();
            let mut row = action(
                id,
                "merge_held",
                (agents, work_ids.clone()),
                if complete {
                    format!(
                        "{held} An owner is still active, so settle the merge order with that owner's agent before merging #{number}. AgentSCM changed nothing."
                    )
                } else {
                    format!(
                        "{held} The Hub could not read every open pull request and hold, so whether every owner is idle is unknown; settle the merge order with the owner's agent before merging #{number}. AgentSCM changed nothing."
                    )
                },
                semantic,
                &who.owner.id,
            );
            // An overlap between agents asks no person.
            row.needs_you = false;
            actions.push(row);
        }
    }
    Ok(actions)
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
    let publications = store.pull_publications(&principal_scope(store, who)?, 0, PULLS)?;
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
    let mut ready = Vec::new();
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
                // Held merges read the pull request's own files, author and activity time.
                capture(&mut evidence, observed.data.as_bytes());
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
                if pr["draft"] == false && green(&found["snapshot"]) {
                    ready.push(number);
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
                        failed_check_text(name, state, number),
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
    if let Some(repo) = &repo
        && !ready.is_empty()
    {
        // A blocker missing from a cut-short read could be active, so nobody is asked then.
        let complete = !["check_pulls_truncated", "pr_observations_truncated"]
            .iter()
            .any(|l| limits.contains(*l));
        actions.extend(held_merges(
            store,
            who,
            &repo.id,
            &people,
            &open,
            &ready,
            complete,
            now,
            &mut limits,
        )?);
    }

    // A pr-gate run whose Stop hook blocks at a gate its agent must act on, once no session
    // that recorded its evidence is live: nobody is working through the gate any more. Only
    // runs on an open pull request or unfinished Work count; older branches are history.
    let scope = principal_scope(store, who)?;
    let repo = crate::hub::skills::Repo {
        name: &who.repository,
        scope: &scope,
    };
    let views = crate::hub::skills::views(store, repo, None, None, RUNS)?;
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
    use super::RecipientKind;
    use super::{failed_check_text, required};
    use crate::hub::store::{self, ForgeIdentity, Store};
    use serde_json::json;
    use std::collections::BTreeMap;

    /// A ready pull request that shares a file with another owner's open one: the other owner
    /// idle asks the waiting person once; recent activity or a live hold asks nobody.
    #[test]
    fn a_merge_held_by_an_idle_owner_asks_its_person_and_an_active_owner_asks_nobody() {
        use crate::hub::connection::NewConnection;
        use crate::hub::intent::NewIntent;
        use crate::hub::store::{agent_token_digest, mint_bound_agent_token};
        use crate::hub::work::WorkStart;
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let store: &dyn Store = &*store;
        let now = store::unix_now();
        let token = |user_id: i64, login: &str| {
            let user = store
                .upsert_user(
                    &ForgeIdentity {
                        provider: "github".into(),
                        host: "github.com".into(),
                        user_id,
                    },
                    login,
                )
                .unwrap();
            let (_, secret, _) =
                mint_bound_agent_token(store, &user.id, login, "acme/widgets", 101, 3600, false)
                    .unwrap();
            store
                .authenticate(&agent_token_digest(secret.expose()).unwrap(), now)
                .unwrap()
                .unwrap()
        };
        let viewer = token(501, "viewer");
        let other = token(502, "other");
        let request: WorkStart = serde_json::from_value(json!({
            "request_key": "ready", "title": "Ready work", "session": "ready-session",
            "scope": ["src/shared.rs"], "branch": "feature/ready"
        }))
        .unwrap();
        let work = store
            .work_start(&viewer.token_id, &request, now)
            .unwrap()
            .item;
        let items = BTreeMap::from([(work.id.clone(), work.clone())]);
        let connection = store
            .insert_connection(
                &NewConnection {
                    id: format!("con_{}", "a".repeat(32)),
                    provider: "github".into(),
                    host: "github.com".into(),
                    kind: "pat".into(),
                    app_id: None,
                    app_slug: None,
                    acts_as: Some("viewer".into()),
                },
                &viewer.owner.id,
            )
            .unwrap();
        let repo = store
            .upsert_repository(
                &connection.id,
                "github.com",
                101,
                "acme/widgets",
                None,
                None,
            )
            .unwrap();
        let (head, base) = ("a".repeat(40), "b".repeat(40));
        let mut stamp = now * 1000;
        let mut observe = |kind: &str, key: &str, value: serde_json::Value| {
            stamp += 1;
            store
                .observe(&repo.id, kind, key, &value.to_string(), stamp)
                .unwrap();
        };
        let ago = |seconds: i64| {
            let (y, mo, d, h, mi, s) = crate::civil::utc_civil(now - seconds);
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
        };
        let pull = |number: i64, login: &str, id: i64, files: &[&str], updated: String| {
            json!({"number": number, "state": "open", "draft": false,
                "head_ref": format!("feature/{login}"), "head_sha": head, "base_sha": base,
                "author": {"login": login, "id": id}, "files": files, "files_truncated": false,
                "updated_at": updated})
        };
        observe(
            "provider_context",
            "_health",
            json!({"availability":"available","invalidated_through":0,
                "permissions":{"pull_requests":"available","checks":"available","statuses":"available"}}),
        );
        // The viewer's pull request on its Work's branch passed every check.
        let mut ready = pull(20, "viewer", 501, &["src/shared.rs", "src/own.rs"], ago(60));
        ready["head_ref"] = json!("feature/ready");
        observe("pull_request", "20", ready);
        observe(
            "provider_context",
            "20",
            json!({"version":1,"provider":"github","repository_id":101,"number":20,
                "head_sha":head,"base_sha":base,
                "check_runs":{"complete":true,"items":[{"name":"tests","state":"completed","conclusion":"success"}]},
                "commit_statuses":{"complete":true,"items":[{"name":"ci","state":"success","head_relation":"same"}]}}),
        );
        let held = |label: &str| {
            let (actions, _, _) = super::read(store, &viewer, &items, now).unwrap();
            let held: Vec<_> = actions
                .into_iter()
                .filter(|a| a.kind == "merge_held")
                .collect();
            assert_eq!(
                held.len(),
                1,
                "{label}: exactly one row per waiting pull request"
            );
            assert_eq!(held[0].id, format!("held:{}:20", repo.id));
            assert_eq!(held[0].work_ids, vec![work.id.clone()]);
            held.into_iter().next().unwrap()
        };

        // Another owner's pull request on the shared file, quiet for five hours.
        observe(
            "pull_request",
            "21",
            pull(21, "other", 502, &["src/shared.rs"], ago(5 * 3600)),
        );
        let idle = held("idle");
        assert!(idle.needs_you);
        assert!(matches!(idle.recipients[0].kind, RecipientKind::Person));
        assert!(
            idle.action
                .contains("#21 by @other (src/shared.rs; no push, review or comment since")
                && idle
                    .action
                    .contains("merge #20 first and leave one comment on each"),
            "{}",
            idle.action
        );

        // A file list the forge cut short could hide an active owner's file: nobody is asked.
        let mut cut = pull(21, "other", 502, &["src/shared.rs"], ago(5 * 3600));
        cut["files_truncated"] = json!(true);
        observe("pull_request", "21", cut);
        let unknown = held("cut file list");
        assert!(!unknown.needs_you);
        assert!(
            unknown
                .action
                .contains("could not read every open pull request"),
            "{}",
            unknown.action
        );

        // A comment ten minutes ago keeps the overlap between agents.
        observe(
            "pull_request",
            "21",
            pull(21, "other", 502, &["src/shared.rs"], ago(600)),
        );
        let active = held("recent activity");
        assert!(!active.needs_you);
        assert!(matches!(
            active.recipients[0].kind,
            RecipientKind::LeadAgent
        ));
        assert!(
            active.action.contains("An owner is still active"),
            "{}",
            active.action
        );

        // Quiet again, but the other owner's agent holds the shared file live.
        observe(
            "pull_request",
            "21",
            pull(21, "other", 502, &["src/shared.rs"], ago(5 * 3600)),
        );
        store
            .announce_intent(
                &other.token_id,
                &NewIntent {
                    id: format!("int_{}", "c".repeat(32)),
                    task: "Synthetic edit".into(),
                    branch: None,
                    base: None,
                    machine: None,
                    paths: vec!["src/shared.rs".into()],
                    ttl_seconds: 3600,
                },
                now,
            )
            .unwrap();
        let holding = held("live hold");
        assert!(!holding.needs_you);
        assert!(
            holding.action.contains("its owner holds them in AgentSCM"),
            "{}",
            holding.action
        );
    }

    #[test]
    fn green_needs_every_check_passed_on_a_complete_read() {
        let snapshot = |runs: serde_json::Value, statuses: serde_json::Value| {
            json!({"check_runs": {"complete": true, "items": runs},
                "commit_statuses": {"complete": true, "items": statuses}})
        };
        let run = |state: &str, conclusion: Option<&str>| json!({"state": state, "conclusion": conclusion});
        let status = |name: &str, state: &str| json!({"name": name, "state": state, "head_relation": "same"});
        assert!(super::green(&snapshot(
            json!([
                run("completed", Some("success")),
                run("completed", Some("skipped"))
            ]),
            // The newest row per context counts: an earlier failure was superseded.
            json!([status("ci", "success"), status("ci", "failure")])
        )));
        for (runs, statuses) in [
            (json!([]), json!([])),
            (json!([run("in_progress", None)]), json!([])),
            (json!([run("completed", Some("failure"))]), json!([])),
            (json!([]), json!([status("ci", "pending")])),
        ] {
            assert!(!super::green(&snapshot(runs, statuses)));
        }
        let mut cut = snapshot(json!([run("completed", Some("success"))]), json!([]));
        cut["commit_statuses"]["complete"] = json!(false);
        assert!(!super::green(&cut));
    }

    #[test]
    fn failed_check_text_is_unchanged_for_legacy_and_hub_and_plain_for_named_contexts() {
        for name in ["agentscm/local-validation", "agentscm/workflow/main"] {
            assert_eq!(
                failed_check_text(name, "failure", 5),
                format!(
                    "Required check {name} reports failure on pull request #5; fix or rerun it before merging."
                )
            );
        }
        assert_eq!(
            failed_check_text("agentscm/local-validation/static", "error", 7),
            "Local validation check agentscm/local-validation/static reports error on pull request #7; fix or rerun it."
        );
    }

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
