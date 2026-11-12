//! Facts for deciding whether a Work's file hold outlived its work: the linked pull request
//! merged and the source issue was closed as completed by that same pull request.
//!
//! The closure is a provider fact the sync stores as an observation. Missing, unreadable or
//! undecodable facts classify as `Unknown` or `MergedOnly`, never as `Settled`. A merge alone
//! never settles a Work: a "Part of" pull request merges while its issue stays open.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::connection::ConnectedRepository;
use super::github::{Gh, GhError};
use super::store::{ForgeIdentity, Principal, Store, User};
use super::work::{MAX_WORK_PAGE, WorkItem, WorkOverviewQuery, WorkSource};
use super::work_lifecycle::OutcomeValue;
use super::work_reconcile::{WorkStage, WorkStageV1, work_stages_detailed};
use super::work_transition::{OperationalSelect, WorkLifecycleReader};

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
    /// The provider id of the repository that pull request belongs to. An observation stored
    /// before it was read has none, and never settles a Work: it is read again instead.
    #[serde(default)]
    pub closer_repository_id: Option<i64>,
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
                closer_repository_id: None,
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
        let (closer_pull, closer_repository_id) = match closer {
            Some(closer) if closer["__typename"].as_str() == Some("PullRequest") => (
                Some(
                    closer["number"]
                        .as_u64()
                        .filter(|n| *n > 0)
                        .ok_or_else(bad)?,
                ),
                Some(
                    closer["repository"]["databaseId"]
                        .as_i64()
                        .filter(|n| *n > 0)
                        .ok_or_else(bad)?,
                ),
            ),
            _ => (None, None),
        };
        Ok(Self {
            state,
            reason,
            closer_pull,
            closer_repository_id,
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
/// stored fact for `issue`, if any; `repository_id` is the Work's repository, which the closing
/// pull request must belong to (a pull request of another repository with the same number is
/// not it).
pub fn classify(
    merged_pull: Option<u64>,
    issue: Option<u64>,
    closure: Option<&IssueClosure>,
    repository_id: i64,
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
            if closure.reason == CloseReason::Completed
                && closure.closer_pull == Some(pull)
                && closure.closer_repository_id == Some(repository_id) =>
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
    /// The Work is listed only because it is unreserved and its branch merged: it holds nothing.
    pub unreserved: bool,
    /// Its linked pull request merged and its read established that every pull request of the
    /// Work merged: no limit on the read, no declared pull request not known merged, and no other
    /// open pull request on its branch.
    pub all_merged: bool,
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

/// Merged facts for `item`. `stage` is its entry from `work_reconcile::work_stages_detailed`
/// and `unsettled` whether that read could not establish that every pull request of the Work
/// merged (a limit on its own read, a declared pull request not known merged, or another open
/// candidate on its branch); such a Work is never `Settled`. `observation_repo` is the connected repository's observation scope (`rep_…`). An unreadable
/// or undecodable closure is treated as unknown.
pub fn facts(
    store: &dyn Store,
    observation_repo: &str,
    item: &WorkItem,
    stage: Option<&WorkStageV1>,
    unsettled: bool,
    provider: &str,
    (full_name, repository_id): (&str, i64),
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
        _ => classify(merged_pull, issue, closure.as_ref(), repository_id),
    };
    let hold = match hold {
        MergedHold::Settled { pull, .. } if unsettled => MergedHold::MergedOnly { pull },
        other => other,
    };
    MergedFacts {
        merged_pull,
        issue,
        closure,
        hold,
        unreserved: false,
        all_merged: merged_pull.is_some() && !unsettled,
    }
}

/// Whether the stored closure of `item`'s source issue says it is closed as completed by a pull
/// request. A cheap, necessary condition for `Settled` that reads one observation and no stages.
pub fn closed_by_pull(
    store: &dyn Store,
    repo: &ConnectedRepository,
    item: &WorkItem,
    provider: &str,
) -> bool {
    source_issue(item, provider, &repo.full_name, repo.repository_id)
        .and_then(|issue| {
            store
                .observation(&repo.id, OBSERVATION_KIND, &observation_key(issue))
                .ok()
                .flatten()
        })
        .and_then(|row| IssueClosure::decode(&row.data))
        .is_some_and(|c| {
            c.state == IssueState::Closed
                && c.reason == CloseReason::Completed
                && c.closer_pull.is_some()
        })
}

/// The merged facts of the unfinished Work of one connected repository.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepositoryFacts {
    /// Ordered by Work id. Work the overview did not list is absent, which is safe: an unlisted
    /// Work is never considered. A limit on one Work's read keeps only that Work from `Settled`.
    pub work: Vec<(WorkItem, MergedFacts)>,
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
    repository_facts_where(store, provider, repo, now, |_| true)
}

/// `repository_facts` for the listed Work `keep` accepts; the stage read, the costly part, runs
/// only for those.
pub fn repository_facts_where(
    store: &dyn Store,
    provider: &str,
    repo: &ConnectedRepository,
    now: i64,
    keep: impl Fn(&WorkItem) -> bool,
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
    // ponytail: at most 100 such Work are read, oldest id first, so a repository with more
    // unreserved merged Work than that leaves the rest unconsidered; upgrade: a cursor.
    let hidden = store.repository_unreserved_merged_work(
        &principal.owner,
        &repo.full_name,
        repo.repository_id,
        MAX_WORK_PAGE,
        now,
    )?;
    let listed: Vec<&WorkItem> = overview
        .current
        .iter()
        .chain(&hidden)
        .filter(|item| keep(item))
        .collect();
    let items: Vec<_> = listed
        .iter()
        .map(|item| (*item, OutcomeValue::None))
        .collect();
    let reader = WorkLifecycleReader::Person {
        owner: &principal.owner,
        repository: &repo.full_name,
        repository_id: repo.repository_id,
    };
    let (stages, _, unsettled) = work_stages_detailed(store, &principal, &reader, &items, now);
    let mut work: Vec<_> = listed
        .iter()
        .map(|item| {
            let mut facts = facts(
                store,
                &repo.id,
                item,
                stages.get(&item.id),
                unsettled.contains(&item.id),
                provider,
                (&repo.full_name, repo.repository_id),
            );
            facts.unreserved = hidden.iter().any(|h| h.id == item.id);
            ((*item).clone(), facts)
        })
        .collect();
    work.sort_by(|a, b| a.0.id.cmp(&b.0.id));
    Ok(RepositoryFacts { work })
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
    work_facts_in(store, principal, &repo, item, now)
}

fn work_facts_in(
    store: &dyn Store,
    principal: &Principal,
    repo: &ConnectedRepository,
    item: &WorkItem,
    now: i64,
) -> Option<MergedFacts> {
    let reader = WorkLifecycleReader::Token(&principal.token_id);
    let (stages, _, unsettled) = work_stages_detailed(
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
        unsettled.contains(&item.id),
        &principal.owner.identity.provider,
        (&repo.full_name, repo.repository_id),
    ))
}

fn release_key(repository_id: i64) -> String {
    format!("merged_release:repo:{repository_id}")
}

/// Whether the repository opted in to releasing the hold of Work its merged pull request
/// finished. Off unless the stored value is exactly `on`; a missing or unreadable value is off.
pub fn merged_release_enabled(store: &dyn Store, repository_id: i64) -> bool {
    store
        .get_setting(&release_key(repository_id))
        .ok()
        .flatten()
        .as_deref()
        == Some("on")
}

/// Switches the repository's automatic release on or off; the caller has checked that `actor`
/// may.
pub fn set_merged_release(
    store: &dyn Store,
    repository_id: i64,
    enabled: bool,
    actor: &str,
) -> super::Result<()> {
    let value = if enabled { "on" } else { "off" };
    store.set_setting(&release_key(repository_id), value, actor)
}

/// The audience under which a policy release is claimed, once per Work ever.
const POLICY_RELEASE: &str = "policy_release";

/// The audience under which the Hub's completion of merged Work is claimed, once per Work ever.
const HUB_COMPLETE: &str = "hub_complete";

/// After a refused policy release: keeps the `policy_release` claim when a re-read shows the
/// Work's hold is gone (someone else released it), else gives the claim back so a later run can
/// release. An unreadable Work counts as still holding. Returns whether the claim was kept.
pub fn settle_refused_release(
    store: &dyn Store,
    reader: &WorkLifecycleReader<'_>,
    work: &str,
    now: i64,
) -> bool {
    let gone = store
        .work_operational(reader, &OperationalSelect::One(work), now)
        .ok()
        .and_then(|o| o.work.first().map(|w| w.reservation.basis.is_none()))
        .unwrap_or(false);
    if !gone {
        let _ = store.unclaim_merge_prompt(work, POLICY_RELEASE);
    }
    gone
}

/// For a repository that opted in, releases the hold of each unfinished Work whose every linked
/// pull request merged and whose source issue the Work's linked pull request closed as completed
/// (`MergedHold::Settled`), when that issue's closure was read and stored in this poll (`read`). It
/// releases the hold only: the Work stays unfinished with no outcome. Returns the ids released.
///
/// The policy releases a Work at most once. It claims the `policy_release` marker (the same
/// once-ever claim the finish prompts use) before it releases, so a lead or owner who then
/// resumes the Work keeps the hold: later polls find the marker taken. The claim comes before the
/// transition so two pollers cannot both release. It is given back when the transition fails
/// with a store error, and when it is refused as stale (the Work changed in some unrelated way)
/// but a re-read still shows the hold, so a later run can release; it is kept only when that
/// re-read shows the hold is gone. A crash between the claim and the transition spends it, which
/// only leaves the hold to lapse by itself, as it would without the policy.
///
/// A Work whose own read hit a limit, that declares a pull request not known merged, or that has
/// another open pull request on its branch is never `Settled`, whatever the rest of the
/// repository looks like. A missing or unreadable closure, an open issue, or an issue closed
/// another way is never `Settled` either, so the Work keeps its hold.
///
/// The release is a transition by the policy actor, fenced by the Work's generation and
/// sequence, so a lead acting at the same moment changes nothing twice: a Work no longer holding
/// is skipped and a stale fence is refused and skipped. A failure on one Work is logged and does
/// not stop the others.
pub fn release_settled(
    store: &dyn Store,
    provider: &str,
    repo: &ConnectedRepository,
    read: &BTreeSet<u64>,
    now: i64,
) -> super::Result<Vec<String>> {
    use super::work_transition::{WorkTransitionActor, WorkTransitionKind, WorkTransitionRequest};
    let mut released = Vec::new();
    if read.is_empty() || !merged_release_enabled(store, repo.repository_id) {
        return Ok(released);
    }
    // A closure this poll did not read again may be stale (the issue reopened), so only Work whose
    // issue was read now is released.
    let found = repository_facts_where(store, provider, repo, now, |item| {
        source_issue(item, provider, &repo.full_name, repo.repository_id)
            .is_some_and(|issue| read.contains(&issue))
    })?;
    for (item, facts) in &found.work {
        let MergedHold::Settled { pull, issue } = facts.hold else {
            continue;
        };
        let attempt = || -> super::Result<bool> {
            let Some(owner) = store.user_by_id(&item.owner)?.filter(|u| !u.disabled) else {
                return Ok(false);
            };
            let reader = WorkLifecycleReader::Person {
                owner: &owner,
                repository: &repo.full_name,
                repository_id: repo.repository_id,
            };
            let overview =
                store.work_operational(&reader, &OperationalSelect::One(&item.id), now)?;
            let Some(current) = overview
                .work
                .first()
                .filter(|w| w.reservation.basis.is_some())
            else {
                return Ok(false);
            };
            if !store.claim_merge_prompt(&item.id, POLICY_RELEASE, now)? {
                return Ok(false);
            }
            let request = WorkTransitionRequest {
                work_id: item.id.clone(),
                operation_id: format!(
                    "repository-policy:{}:{}:{}",
                    item.id, current.generation, current.updated_seq
                ),
                generation: current.generation,
                expected_updated_seq: current.updated_seq,
                session: None,
                transition: WorkTransitionKind::Release,
                reason: format!(
                    "Released by repository policy: pull request #{pull} merged and closed #{issue} as completed."
                ),
                superseded_by: None,
                participants: Vec::new(),
            };
            let actor = WorkTransitionActor::Policy {
                owner: &owner,
                repository: &repo.full_name,
                repository_id: repo.repository_id,
            };
            match store.work_transition(&actor, &request, now) {
                Ok(_) => Ok(true),
                // The fence moved or the Work no longer holds. The claim stays only when a
                // re-read shows the hold is gone; otherwise it is given back for a later run.
                Err(error @ super::HubError::Invalid(_)) => {
                    settle_refused_release(store, &reader, &item.id, now);
                    Err(error)
                }
                Err(error) => {
                    let _ = store.unclaim_merge_prompt(&item.id, POLICY_RELEASE);
                    Err(error)
                }
            }
        };
        match attempt() {
            Ok(true) => released.push(item.id.clone()),
            Ok(false) => {}
            // A fence the Work moved past, or a hold already gone: nothing to release.
            Err(super::HubError::Invalid(_)) => {}
            Err(error) => super::error_envelope::log_internal(&error),
        }
    }
    Ok(released)
}

/// Whether the Hub completes `facts`' Work on its observed merge: every pull request of the Work
/// merged, and its source issue, when it has one, was closed as completed by that pull request.
/// A "Part of" pull request merges while its issue stays open, so a Work with an issue that is
/// open, unread or closed another way is never completed.
fn completes_on_merge(facts: &MergedFacts) -> Option<u64> {
    if !facts.all_merged {
        return None;
    }
    match facts.hold {
        MergedHold::Settled { pull, .. } => Some(pull),
        MergedHold::MergedOnly { pull } if facts.issue.is_none() => Some(pull),
        _ => None,
    }
}

/// Completes each unfinished Work of `repo` whose pull request the Hub observed merge, so Work
/// whose agent ended its turn before the merge queue merged does not stay open holding files.
/// Returns the ids completed.
///
/// A Work is completed when `completes_on_merge` accepts its facts, it is active or paused (a
/// blocked Work waits on someone and a handed-over Work on its next lead), and no live session
/// holds it: its reservation lapsed or it holds none. While its lead's lease is live the Hub
/// leaves it to the lead, whom the merged prompt tells to complete it, and completes it after
/// the lease lapses. The Hub completes a Work at most once ever (the `hub_complete` marker), so a
/// lead who reopens it keeps it open, and it leaves alone a Work its lead resumed after the
/// opted-in policy released it.
///
/// The completion is a `complete` transition by the policy actor for the Work's owner, fenced
/// by the Work's generation and sequence, with an operation id naming both, so a lead finishing
/// at the same moment changes nothing twice and a retry replays. The outcome reads
/// `hub_observed_merge`. A failure on one Work is logged and does not stop the others.
pub fn complete_merged(
    store: &dyn Store,
    provider: &str,
    repo: &ConnectedRepository,
    now: i64,
) -> super::Result<Vec<String>> {
    use super::work::WorkState;
    use super::work_lifecycle::ActivityStatus;
    use super::work_transition::{WorkTransitionActor, WorkTransitionKind, WorkTransitionRequest};
    let found = repository_facts_where(store, provider, repo, now, |item| {
        matches!(item.state, WorkState::Active | WorkState::Paused)
    })?;
    let mut completed = Vec::new();
    for (item, facts) in &found.work {
        let Some(pull) = completes_on_merge(facts) else {
            continue;
        };
        let attempt = || -> super::Result<bool> {
            let Some(owner) = store.user_by_id(&item.owner)?.filter(|u| !u.disabled) else {
                return Ok(false);
            };
            let reader = WorkLifecycleReader::Person {
                owner: &owner,
                repository: &repo.full_name,
                repository_id: repo.repository_id,
            };
            let overview =
                store.work_operational(&reader, &OperationalSelect::One(&item.id), now)?;
            let open = |w: &super::work_lifecycle::WorkOperationalV1| {
                matches!(w.state, WorkState::Active | WorkState::Paused)
            };
            let Some(current) = overview
                .work
                .first()
                .filter(|w| open(w) && w.activity.status != ActivityStatus::Live)
            else {
                return Ok(false);
            };
            // A Work its lead resumed after the policy released it holds again on purpose.
            if current.reservation.basis.is_some()
                && store.merge_prompt_claimed(&item.id, POLICY_RELEASE)?
            {
                return Ok(false);
            }
            // Once ever, claimed before the transition so two pollers cannot both complete, and so
            // Work reopened after the Hub completed it is left to its lead.
            if !store.claim_merge_prompt(&item.id, HUB_COMPLETE, now)? {
                return Ok(false);
            }
            let request = WorkTransitionRequest {
                work_id: item.id.clone(),
                operation_id: format!(
                    "hub-observed-merge:{}:{}:{}",
                    item.id, current.generation, current.updated_seq
                ),
                generation: current.generation,
                expected_updated_seq: current.updated_seq,
                session: None,
                transition: WorkTransitionKind::Complete,
                reason: format!("Completed by the Hub: it observed pull request #{pull} merge."),
                superseded_by: None,
                participants: Vec::new(),
            };
            let actor = WorkTransitionActor::Policy {
                owner: &owner,
                repository: &repo.full_name,
                repository_id: repo.repository_id,
            };
            let done = store.work_transition(&actor, &request, now);
            if done.is_err() {
                // The claim stays only when a re-read shows the Work is no longer open (someone
                // finished it); otherwise it is given back so a later poll can complete it.
                let still_open = store
                    .work_operational(&reader, &OperationalSelect::One(&item.id), now)
                    .map_or(true, |o| o.work.first().is_some_and(open));
                if still_open {
                    let _ = store.unclaim_merge_prompt(&item.id, HUB_COMPLETE);
                }
            }
            done.map(|_| true)
        };
        match attempt() {
            Ok(true) => completed.push(item.id.clone()),
            Ok(false) => {}
            // The fence moved or the Work finished meanwhile: a later poll reads it again.
            Err(super::HubError::Invalid(_)) => {}
            Err(error) => super::error_envelope::log_internal(&error),
        }
    }
    Ok(completed)
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

impl MergedNotice {
    /// The text this notice shows once claimed.
    pub fn line(&self) -> &str {
        &self.line
    }
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
    // Cheapest first: Work already told, then Work whose stored issue closure cannot settle it.
    let led: Vec<_> = led
        .into_iter()
        .filter(|item| !store.merge_prompt_claimed(&item.id, "lead").unwrap_or(true))
        .collect();
    if led.is_empty() {
        return Vec::new();
    }
    let Ok(Some(repo)) = super::sync::find_repository(
        store,
        &principal.owner.identity.host,
        &principal.repository,
        principal.repository_id,
    ) else {
        return Vec::new();
    };
    led.into_iter()
        .filter(|item| closed_by_pull(store, &repo, item, &principal.owner.identity.provider))
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
                    " as completed by it. Complete it (agentscm_done with outcome complete), or if work remains, release the hold and continue later.",
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
    let provider = &principal.owner.identity.provider;
    // Work this owner owns that is not yet told and whose stored closure could settle it; only
    // those have their stages read.
    let Ok(facts) = repository_facts_where(store, provider, &repo, now, |item| {
        item.owner == principal.owner.id
            && !store
                .merge_prompt_claimed(&item.id, "owner")
                .unwrap_or(true)
            && closed_by_pull(store, &repo, item, provider)
    }) else {
        return Vec::new();
    };
    facts
        .work
        .into_iter()
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

/// The most conflicts `annotate_conflicts` reads facts for.
// ponytail: five conflicts, lowest Work id first; upgrade: a count line when more are held.
const MAX_ANNOTATED_CONFLICTS: usize = 5;

/// Adds the merged facts to the `next` text of each conflict whose holding Work has a merged
/// linked pull request, for at most the first few conflicts by Work id. When a caller that can
/// write owns that Work (`mine`), it also names the direct release remedy: preview, then apply,
/// with no join request. Other conflicts are unchanged, and a Work that cannot be read or has no
/// connected repository adds nothing.
pub fn annotate_conflicts(
    store: &dyn Store,
    principal: &Principal,
    recovery: &mut super::work_recovery::WorkRecoveryV1,
    now: i64,
) {
    let Ok(Some(repo)) = super::sync::find_repository(
        store,
        &principal.owner.identity.host,
        &principal.repository,
        principal.repository_id,
    ) else {
        return;
    };
    let mut order: Vec<usize> = (0..recovery.conflicts.len()).collect();
    order.sort_by(|a, b| {
        recovery.conflicts[*a]
            .work_id
            .cmp(&recovery.conflicts[*b].work_id)
    });
    for index in order.into_iter().take(MAX_ANNOTATED_CONFLICTS) {
        let conflict = &mut recovery.conflicts[index];
        let Ok(Some(item)) = store.work_get(&principal.token_id, &conflict.work_id, now) else {
            continue;
        };
        let Some(hold) = work_facts_in(store, principal, &repo, &item, now).map(|facts| facts.hold)
        else {
            continue;
        };
        let Some(sentence) = merged_sentence(&hold) else {
            continue;
        };
        conflict.next.push(' ');
        conflict.next.push_str(&sentence);
        if conflict.mine && !principal.read_only {
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

    const REPO: i64 = 42;

    const fn closed(reason: CloseReason, closer: Option<u64>) -> IssueClosure {
        IssueClosure {
            state: IssueState::Closed,
            reason,
            closer_pull: closer,
            closer_repository_id: match closer {
                Some(_) => Some(REPO),
                None => None,
            },
        }
    }

    #[test]
    fn settled_only_when_the_merged_pull_closed_the_issue_as_completed() {
        let c = closed(CloseReason::Completed, Some(7));
        assert_eq!(
            classify(Some(7), Some(3), Some(&c), REPO),
            MergedHold::Settled { pull: 7, issue: 3 }
        );
    }

    #[test]
    fn a_closer_of_another_repository_or_unread_repository_never_settles() {
        let mut c = closed(CloseReason::Completed, Some(7));
        c.closer_repository_id = Some(REPO + 1);
        assert_eq!(
            classify(Some(7), Some(3), Some(&c), REPO),
            MergedHold::MergedOnly { pull: 7 }
        );
        // An observation stored before the repository was read decodes without it.
        let old =
            IssueClosure::decode(r#"{"state":"closed","reason":"completed","closer_pull":7}"#)
                .unwrap();
        assert_eq!(old.closer_repository_id, None);
        assert_eq!(
            classify(Some(7), Some(3), Some(&old), REPO),
            MergedHold::MergedOnly { pull: 7 }
        );
    }

    #[test]
    fn part_of_pull_leaves_the_issue_open_and_unprompted() {
        let open = IssueClosure {
            state: IssueState::Open,
            reason: CloseReason::Other,
            closer_pull: None,
            closer_repository_id: None,
        };
        assert_eq!(
            classify(Some(7), Some(3), Some(&open), REPO),
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
                classify(Some(7), Some(3), Some(&c), REPO),
                MergedHold::MergedOnly { pull: 7 },
                "{c:?}"
            );
        }
    }

    #[test]
    fn missing_facts_never_settle() {
        assert_eq!(
            classify(Some(7), Some(3), None, REPO),
            MergedHold::MergedOnly { pull: 7 }
        );
        assert_eq!(
            classify(
                Some(7),
                None,
                Some(&closed(CloseReason::Completed, Some(7))),
                REPO
            ),
            MergedHold::MergedOnly { pull: 7 }
        );
        assert_eq!(
            classify(
                None,
                Some(3),
                Some(&closed(CloseReason::Completed, Some(7))),
                REPO
            ),
            MergedHold::Unknown
        );
        assert_eq!(classify(None, None, None, REPO), MergedHold::Unknown);
    }

    fn reply(state: &str, reason: Value, nodes: Value) -> Value {
        json!({"data": {"repository": {"issue": {
            "state": state, "stateReason": reason,
            "timelineItems": {"nodes": nodes}}}}})
    }

    fn merged(hold: MergedHold, issue: Option<u64>, all_merged: bool) -> MergedFacts {
        MergedFacts {
            merged_pull: Some(7),
            issue,
            closure: None,
            hold,
            unreserved: false,
            all_merged,
        }
    }

    #[test]
    fn completes_only_a_fully_merged_work_whose_issue_it_closed_or_that_has_none() {
        let settled = MergedHold::Settled { pull: 7, issue: 3 };
        let only = MergedHold::MergedOnly { pull: 7 };
        assert_eq!(completes_on_merge(&merged(settled, Some(3), true)), Some(7));
        assert_eq!(completes_on_merge(&merged(only, None, true)), Some(7));
        // Another pull request open or unread, an issue left open, unread or closed otherwise.
        assert_eq!(completes_on_merge(&merged(only, None, false)), None);
        assert_eq!(completes_on_merge(&merged(only, Some(3), true)), None);
        let open = MergedHold::MergedIssueOpen { pull: 7, issue: 3 };
        assert_eq!(completes_on_merge(&merged(open, Some(3), true)), None);
        for hold in [MergedHold::NotMerged, MergedHold::Unknown] {
            assert_eq!(completes_on_merge(&merged(hold, None, true)), None);
        }
    }

    #[test]
    fn decodes_a_pull_request_closer() {
        let body = reply(
            "CLOSED",
            json!("COMPLETED"),
            json!([{"closer": {"__typename": "PullRequest", "number": 12,
                "repository": {"databaseId": 42}}}]),
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
            reply(
                "CLOSED",
                json!("COMPLETED"),
                json!([{"closer": {"__typename": "PullRequest", "number": 12}}]),
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
