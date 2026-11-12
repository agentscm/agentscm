//! Facts for deciding whether a Work's file hold outlived its work: the linked pull request
//! merged and the source issue was closed as completed by that same pull request.
//!
//! The closure is a provider fact the sync stores as an observation. Missing, unreadable or
//! undecodable facts classify as `Unknown` or `MergedOnly`, never as `Settled`. A merge alone
//! never settles a Work: a "Part of" pull request merges while its issue stays open.
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::connection::ConnectedRepository;
use super::github::{Gh, GhError};
use super::store::{ForgeIdentity, Principal, Store, User};
use super::work::{MAX_WORK_PAGE, WorkItem, WorkOverviewQuery, WorkSource};
use super::work_lifecycle::OutcomeValue;
use super::work_reconcile::{WorkStage, WorkStageV1, work_stages};
use super::work_transition::WorkLifecycleReader;

/// Observation kind and key prefix of one issue's closure fact.
pub const OBSERVATION_KIND: &str = "issue_closure";

/// The observation key of an issue.
pub fn observation_key(issue: u64) -> String {
    format!("issue:{issue}")
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Completed,
    NotPlanned,
    /// Duplicate, reopened, absent or any reason this model does not name.
    Other,
}

/// How a source issue stands, as of one provider read.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssueClosure {
    pub state: IssueState,
    pub reason: CloseReason,
    /// The pull request that closed the issue, when its latest closing event names one.
    #[serde(default)]
    pub closer_pull: Option<u64>,
}

impl IssueClosure {
    /// Strict decode of a stored observation; `None` when it does not fit the model.
    pub fn decode(data: &str) -> Option<Self> {
        serde_json::from_str(data).ok()
    }

    /// Decode a GitHub GraphQL reply for the one-issue query. Anything unexpected is an
    /// error, so a partial reply can never be stored as a fact.
    pub fn from_graphql(body: &Value) -> Gh<Self> {
        let bad = || GhError::Failed("unreadable issue closure reply".into());
        let issue = &body["data"]["repository"]["issue"];
        let state = match issue["state"].as_str().ok_or_else(bad)? {
            "OPEN" => IssueState::Open,
            "CLOSED" => IssueState::Closed,
            _ => return Err(bad()),
        };
        if state == IssueState::Open {
            return Ok(Self {
                state,
                reason: CloseReason::Other,
                closer_pull: None,
            });
        }
        let reason = match issue["stateReason"].as_str() {
            Some("COMPLETED") => CloseReason::Completed,
            Some("NOT_PLANNED") => CloseReason::NotPlanned,
            _ => CloseReason::Other,
        };
        let nodes = issue["timelineItems"]["nodes"].as_array().ok_or_else(bad)?;
        // Only the latest closing event counts; a Commit or other closer names no pull request.
        let closer = nodes.last().map(|node| &node["closer"]);
        let closer_pull = match closer {
            Some(closer) if closer["__typename"].as_str() == Some("PullRequest") => Some(
                closer["number"]
                    .as_u64()
                    .filter(|n| *n > 0)
                    .ok_or_else(bad)?,
            ),
            _ => None,
        };
        Ok(Self {
            state,
            reason,
            closer_pull,
        })
    }
}

/// What the merged facts of one Work say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergedHold {
    /// Linked pull request merged and it closed the source issue as completed.
    Settled { pull: u64, issue: u64 },
    /// Linked pull request merged and the source issue is still open ("Part of").
    MergedIssueOpen { pull: u64, issue: u64 },
    /// Linked pull request merged; the issue is absent, unknown or closed some other way.
    MergedOnly { pull: u64 },
    /// The Work has a known link that is not merged.
    NotMerged,
    /// Not enough is known.
    Unknown,
}

/// Classify. `merged_pull` is the Work's linked pull request when it merged; `closure` is the
/// stored fact for `issue`, if any.
pub fn classify(
    merged_pull: Option<u64>,
    issue: Option<u64>,
    closure: Option<&IssueClosure>,
) -> MergedHold {
    let Some(pull) = merged_pull else {
        return MergedHold::Unknown;
    };
    let (Some(issue), Some(closure)) = (issue, closure) else {
        return MergedHold::MergedOnly { pull };
    };
    match closure.state {
        IssueState::Open => MergedHold::MergedIssueOpen { pull, issue },
        IssueState::Closed
            if closure.reason == CloseReason::Completed && closure.closer_pull == Some(pull) =>
        {
            MergedHold::Settled { pull, issue }
        }
        IssueState::Closed => MergedHold::MergedOnly { pull },
    }
}

/// The merged facts of one Work.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedFacts {
    pub merged_pull: Option<u64>,
    pub issue: Option<u64>,
    pub closure: Option<IssueClosure>,
    pub hold: MergedHold,
}

/// The Work's source issue number in `repository_id`/`full_name`, from its forge source or its
/// declared issue link. A source in another repository names no issue here.
pub fn source_issue(
    item: &WorkItem,
    provider: &str,
    full_name: &str,
    repository_id: i64,
) -> Option<u64> {
    if let WorkSource::Forge {
        provider: p,
        repository,
        issue,
        ..
    } = &item.source
        && p == provider
        && repository == full_name
    {
        return issue
            .trim_start_matches('#')
            .parse()
            .ok()
            .filter(|n| *n > 0);
    }
    let link = item.issue_link.as_ref()?;
    (link.provider == provider && link.repository_id == repository_id)
        .then(|| u64::try_from(link.number).ok().filter(|n| *n > 0))
        .flatten()
}

/// Merged facts for `item`. `stage` is its entry from `work_reconcile::work_stages`;
/// `observation_repo` is the connected repository's observation scope (`rep_…`). An unreadable
/// or undecodable closure is treated as unknown.
pub fn facts(
    store: &dyn Store,
    observation_repo: &str,
    item: &WorkItem,
    stage: Option<&WorkStageV1>,
    provider: &str,
    full_name: &str,
    repository_id: i64,
) -> MergedFacts {
    let merged_pull = stage
        .filter(|s| s.stage == WorkStage::PullRequestMerged)
        .and_then(|s| s.pull_request)
        .and_then(|n| u64::try_from(n).ok());
    let issue = source_issue(item, provider, full_name, repository_id);
    let closure = issue.and_then(|issue| {
        store
            .observation(observation_repo, OBSERVATION_KIND, &observation_key(issue))
            .ok()
            .flatten()
            .and_then(|row| IssueClosure::decode(&row.data))
    });
    let hold = match (stage, merged_pull) {
        (Some(s), None) if matches!(s.stage, WorkStage::Working | WorkStage::PullRequestOpen) => {
            MergedHold::NotMerged
        }
        _ => classify(merged_pull, issue, closure.as_ref()),
    };
    MergedFacts {
        merged_pull,
        issue,
        closure,
        hold,
    }
}

/// The merged facts of the unfinished Work of one connected repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryFacts {
    /// Ordered by Work id.
    pub work: Vec<(WorkItem, MergedFacts)>,
    /// False when the read listed fewer Work than the repository has, or a stage limit was hit.
    pub complete: bool,
}

/// The repository's own scope as a `Principal`, for the stage logic that reads by repository
/// name and id. It holds no token and cannot write; it is never handed to a caller.
fn repository_principal(provider: &str, repo: &ConnectedRepository) -> Principal {
    Principal {
        token_id: String::new(),
        name: String::new(),
        repository: repo.full_name.clone(),
        repository_id: Some(repo.repository_id),
        expires_at: i64::MAX,
        read_only: true,
        owner: User {
            id: String::new(),
            identity: ForgeIdentity {
                provider: provider.to_owned(),
                host: repo.host.clone(),
                user_id: 0,
            },
            login: String::new(),
            name: None,
            disabled: false,
        },
    }
}

/// Merged facts for every unfinished Work in `repo` that the repository overview lists: Work
/// holding a reservation, and Work whose branch has not merged. `provider` is the connection's
/// provider (`github`). Callers run this in the Hub, where the repository scope needs no token.
// ponytail: one overview page (100 newest Work); upgrade: continue by `after` cursor.
pub fn repository_facts(
    store: &dyn Store,
    provider: &str,
    repo: &ConnectedRepository,
    now: i64,
) -> super::Result<RepositoryFacts> {
    let principal = repository_principal(provider, repo);
    let overview = store.repository_work_overview(
        &principal.owner,
        &repo.full_name,
        repo.repository_id,
        &WorkOverviewQuery {
            recent_since: None,
            limit: MAX_WORK_PAGE,
            after: None,
        },
        now,
    )?;
    // The overview hides unreserved Work whose branch merged; the merged facts are about it.
    let hidden = store.repository_unreserved_merged_work(
        &principal.owner,
        &repo.full_name,
        repo.repository_id,
        MAX_WORK_PAGE,
        now,
    )?;
    let listed: Vec<&WorkItem> = overview.current.iter().chain(&hidden).collect();
    let items: Vec<_> = listed
        .iter()
        .map(|item| (*item, OutcomeValue::None))
        .collect();
    let reader = WorkLifecycleReader::Person {
        owner: &principal.owner,
        repository: &repo.full_name,
        repository_id: repo.repository_id,
    };
    let (stages, limits) = work_stages(store, &principal, &reader, &items, now);
    let mut work: Vec<_> = listed
        .iter()
        .map(|item| {
            let facts = facts(
                store,
                &repo.id,
                item,
                stages.get(&item.id),
                provider,
                &repo.full_name,
                repo.repository_id,
            );
            ((*item).clone(), facts)
        })
        .collect();
    work.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    Ok(RepositoryFacts {
        work,
        complete: overview.current_complete && limits.is_empty(),
    })
}

/// Merged facts of one Work for a caller holding a `Principal` (an MCP or hook request). The
/// observation scope is the connected repository found by the principal's host, repository name
/// and id (`sync::find_repository`), whose `rep_` id keys the closure observation. `None` when
/// that repository is not connected.
pub fn work_facts(
    store: &dyn Store,
    principal: &Principal,
    item: &WorkItem,
    now: i64,
) -> Option<MergedFacts> {
    let repo = super::sync::find_repository(
        store,
        &principal.owner.identity.host,
        &principal.repository,
        principal.repository_id,
    )
    .ok()??;
    let reader = WorkLifecycleReader::Token(&principal.token_id);
    let (stages, _) = work_stages(
        store,
        principal,
        &reader,
        &[(item, OutcomeValue::None)],
        now,
    );
    Some(facts(
        store,
        &repo.id,
        item,
        stages.get(&item.id),
        &principal.owner.identity.provider,
        &repo.full_name,
        repo.repository_id,
    ))
}

/// The most owner start lines one session start carries.
// ponytail: ten lines, oldest Work id first; upgrade: a count line and a page when more wait.
const MAX_START_LINES: usize = 10;

fn merged_line(item: &WorkItem, pull: u64, issue: u64, ending: &str) -> String {
    format!(
        "AgentSCM: pull request #{pull} for Work {id} \"{title}\" merged and #{issue} is closed{ending}",
        id = item.id,
        title = super::advice::sanitize(&item.title, 80),
    )
}

/// A notice not yet delivered: `claim` it, and show the line only when the claim is won.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedNotice {
    work: String,
    /// The audience the line is for (`lead` or `owner`).
    audience: &'static str,
    /// Audiences claimed with it, so the same session is not told twice.
    also: Option<&'static str>,
    line: String,
}

/// Claims each notice once ever and returns the lines of those this call won. Run it only when
/// the lines will be delivered: the claim is the delivery.
pub fn claim_notices(store: &dyn Store, notices: Vec<MergedNotice>, now: i64) -> Vec<String> {
    let mut lines = Vec::new();
    for notice in notices {
        if !store
            .claim_merge_prompt(&notice.work, notice.audience, now)
            .unwrap_or(false)
        {
            continue;
        }
        if let Some(also) = notice.also {
            let _ = store.claim_merge_prompt(&notice.work, also, now);
        }
        lines.push(notice.line);
    }
    lines
}

/// The unclaimed notice for each unfinished Work that `session` of `principal` leads whose merged
/// pull request closed its source issue.
pub fn lead_notices(
    store: &dyn Store,
    principal: &Principal,
    session: &str,
    now: i64,
) -> Vec<MergedNotice> {
    let Ok(led) = store.session_led_work(&principal.token_id, session) else {
        return Vec::new();
    };
    led.into_iter()
        .filter_map(|item| {
            let hold = work_facts(store, principal, &item, now)?.hold;
            let MergedHold::Settled { pull, issue } = hold else {
                return None;
            };
            Some(MergedNotice {
                work: item.id.clone(),
                audience: "lead",
                also: None,
                line: merged_line(
                    &item,
                    pull,
                    issue,
                    " as completed by it. Finish it (agentscm_done with outcome complete) or release it if work remains.",
                ),
            })
        })
        .collect()
}

/// The unclaimed notice for each unfinished Work in the principal's repository that its owner
/// owns, held or not, whose merged pull request closed its source issue. `session` is the session
/// that is starting: when it leads such Work, the lead notice is claimed with the owner's.
pub fn owner_start_notices(
    store: &dyn Store,
    principal: &Principal,
    session: &str,
    now: i64,
) -> Vec<MergedNotice> {
    let Ok(Some(repo)) = super::sync::find_repository(
        store,
        &principal.owner.identity.host,
        &principal.repository,
        principal.repository_id,
    ) else {
        return Vec::new();
    };
    let Ok(facts) = repository_facts(store, &principal.owner.identity.provider, &repo, now) else {
        return Vec::new();
    };
    facts
        .work
        .into_iter()
        .filter(|(item, _)| item.owner == principal.owner.id)
        .filter_map(|(item, facts)| {
            let MergedHold::Settled { pull, issue } = facts.hold else {
                return None;
            };
            let leads = item.lead_token == principal.token_id && item.lead_session == session;
            Some(MergedNotice {
                work: item.id.clone(),
                audience: "owner",
                also: leads.then_some("lead"),
                line: merged_line(&item, pull, issue, "; complete it or say what is left."),
            })
        })
        .take(MAX_START_LINES)
        .collect()
}

/// The sentence a refusal adds when the holding Work's linked pull request merged. Numbers only:
/// nothing user-controlled enters the text. `None` when no linked pull request merged.
pub fn merged_sentence(hold: &MergedHold) -> Option<String> {
    match *hold {
        MergedHold::Settled { pull, issue } => Some(format!(
            "Its pull request #{pull} merged and #{issue} is closed as completed by it."
        )),
        MergedHold::MergedIssueOpen { pull, issue } => Some(format!(
            "Its pull request #{pull} merged; #{issue} is still open, so work may remain."
        )),
        MergedHold::MergedOnly { pull } => Some(format!("Its pull request #{pull} merged.")),
        MergedHold::NotMerged | MergedHold::Unknown => None,
    }
}

/// Adds the merged facts to the `next` text of each conflict whose holding Work has a merged
/// linked pull request. When the caller owns that Work (`mine`), it also names the direct
/// release remedy: preview, then apply, with no join request. Other conflicts are unchanged, and
/// a Work that cannot be read or has no connected repository adds nothing.
pub fn annotate_conflicts(
    store: &dyn Store,
    principal: &Principal,
    recovery: &mut super::work_recovery::WorkRecoveryV1,
    now: i64,
) {
    for conflict in &mut recovery.conflicts {
        let Ok(Some(item)) = store.work_get(&principal.token_id, &conflict.work_id, now) else {
            continue;
        };
        let Some(hold) = work_facts(store, principal, &item, now).map(|facts| facts.hold) else {
            continue;
        };
        let Some(sentence) = merged_sentence(&hold) else {
            continue;
        };
        conflict.next.push(' ');
        conflict.next.push_str(&sentence);
        if conflict.mine {
            let obsolete = if matches!(hold, MergedHold::MergedIssueOpen { .. }) {
                "Only if it is obsolete, release"
            } else {
                "To free these paths, release"
            };
            conflict.next.push_str(&format!(
                " {obsolete} it without a join request: agentscm_work_recovery_preview with work_id {id}, remedy release and your session, then send the apply call it returns (agentscm_work_transition).",
                id = conflict.work_id
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const fn closed(reason: CloseReason, closer: Option<u64>) -> IssueClosure {
        IssueClosure {
            state: IssueState::Closed,
            reason,
            closer_pull: closer,
        }
    }

    #[test]
    fn settled_only_when_the_merged_pull_closed_the_issue_as_completed() {
        let c = closed(CloseReason::Completed, Some(7));
        assert_eq!(
            classify(Some(7), Some(3), Some(&c)),
            MergedHold::Settled { pull: 7, issue: 3 }
        );
    }

    #[test]
    fn part_of_pull_leaves_the_issue_open_and_unprompted() {
        let open = IssueClosure {
            state: IssueState::Open,
            reason: CloseReason::Other,
            closer_pull: None,
        };
        assert_eq!(
            classify(Some(7), Some(3), Some(&open)),
            MergedHold::MergedIssueOpen { pull: 7, issue: 3 }
        );
    }

    #[test]
    fn closed_otherwise_is_merged_only() {
        for c in [
            closed(CloseReason::NotPlanned, Some(7)),
            closed(CloseReason::Other, Some(7)),
            closed(CloseReason::Completed, Some(8)),
            closed(CloseReason::Completed, None),
        ] {
            assert_eq!(
                classify(Some(7), Some(3), Some(&c)),
                MergedHold::MergedOnly { pull: 7 },
                "{c:?}"
            );
        }
    }

    #[test]
    fn missing_facts_never_settle() {
        assert_eq!(
            classify(Some(7), Some(3), None),
            MergedHold::MergedOnly { pull: 7 }
        );
        assert_eq!(
            classify(
                Some(7),
                None,
                Some(&closed(CloseReason::Completed, Some(7)))
            ),
            MergedHold::MergedOnly { pull: 7 }
        );
        assert_eq!(
            classify(
                None,
                Some(3),
                Some(&closed(CloseReason::Completed, Some(7)))
            ),
            MergedHold::Unknown
        );
        assert_eq!(classify(None, None, None), MergedHold::Unknown);
    }

    fn reply(state: &str, reason: Value, nodes: Value) -> Value {
        json!({"data": {"repository": {"issue": {
            "state": state, "stateReason": reason,
            "timelineItems": {"nodes": nodes}}}}})
    }

    #[test]
    fn decodes_a_pull_request_closer() {
        let body = reply(
            "CLOSED",
            json!("COMPLETED"),
            json!([{"closer": {"__typename": "PullRequest", "number": 12}}]),
        );
        assert_eq!(
            IssueClosure::from_graphql(&body).unwrap(),
            closed(CloseReason::Completed, Some(12))
        );
    }

    #[test]
    fn a_commit_or_null_closer_names_no_pull_request() {
        for node in [
            json!({"closer": {"__typename": "Commit"}}),
            json!({"closer": null}),
            json!({}),
        ] {
            let body = reply("CLOSED", json!("NOT_PLANNED"), json!([node]));
            assert_eq!(
                IssueClosure::from_graphql(&body).unwrap(),
                closed(CloseReason::NotPlanned, None)
            );
        }
        let body = reply("CLOSED", Value::Null, json!([]));
        assert_eq!(
            IssueClosure::from_graphql(&body).unwrap(),
            closed(CloseReason::Other, None)
        );
    }

    #[test]
    fn open_issue_and_malformed_replies() {
        let open = reply("OPEN", Value::Null, json!([]));
        assert_eq!(
            IssueClosure::from_graphql(&open).unwrap().state,
            IssueState::Open
        );
        for bad in [
            json!({"data": {"repository": {"issue": null}}}),
            json!({"errors": [{"type": "NOT_FOUND"}]}),
            reply("MERGED", Value::Null, json!([])),
            reply("CLOSED", json!("COMPLETED"), Value::Null),
            reply(
                "CLOSED",
                json!("COMPLETED"),
                json!([{"closer": {"__typename": "PullRequest"}}]),
            ),
        ] {
            assert!(IssueClosure::from_graphql(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn stored_closure_decoding_is_strict() {
        let ok = serde_json::to_string(&closed(CloseReason::Completed, Some(7))).unwrap();
        assert_eq!(
            IssueClosure::decode(&ok),
            Some(closed(CloseReason::Completed, Some(7)))
        );
        assert_eq!(
            IssueClosure::decode(r#"{"state":"closed","reason":"completed","extra":1}"#),
            None
        );
        assert_eq!(
            IssueClosure::decode(r#"{"state":"gone","reason":"completed"}"#),
            None
        );
    }
}
