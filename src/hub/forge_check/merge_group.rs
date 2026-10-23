//! The Hub workflow status on a GitHub merge queue group.
//!
//! A merge group is a commit GitHub builds on a `gh-readonly-queue/<base>/pr-<n>-<sha>` branch:
//! the base (or the groups ahead of it) combined with one queued pull request. Its bytes can
//! differ from every pull request head, so a head success never carries over. Success needs the
//! group commit's own tree to equal the tree the pinned native store qualified, on the very base
//! the candidate was qualified against. A group stacked on other queued changes has no evidence
//! for its combined source and stays pending. The webhook payload only names the group; its
//! existence, tree and base are read again from GitHub.
use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::super::connection::{
    ConnectedRepository, Connection, ForgeCheckReserve, MergeGroup, NewForgeCheck, NewMergeGroup,
};
use super::super::forge_api::ForgeApi;
use super::super::github::{self, CommitStatus, Gh, GhError, StatusState};
use super::super::http::blocking;
use super::super::publish::Publication;
use super::super::store::unix_now;
use super::super::workspace_execution::AgentTaskProfile;
use super::super::{HubState, Result};
use super::{
    DISABLED, NativeState, StatusAuthorization, Verdict, field, is_oid, may_show_success,
    newest_publication, publication_native, status_authorized, status_context,
};
use crate::candidate::CandidateSubject;

const QUEUE_PREFIX: &str = "gh-readonly-queue/";

/// The pull request a queue branch names: `gh-readonly-queue/<base>/pr-<number>-<head sha>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueBranch {
    pub base: String,
    pub pr_number: i64,
    pub pr_head_sha: String,
}

/// Parses a queue branch name, with or without `refs/heads/`. None for anything else.
pub fn queue_branch(name: &str) -> Option<QueueBranch> {
    let name = name.strip_prefix("refs/heads/").unwrap_or(name);
    let rest = name.strip_prefix(QUEUE_PREFIX)?;
    let (base, entry) = rest.rsplit_once('/')?;
    let (number, sha) = entry.strip_prefix("pr-")?.split_once('-')?;
    let pr_number = number.parse::<i64>().ok().filter(|n| *n > 0)?;
    let valid = !base.is_empty()
        && super::super::github::valid_ref_name(base)
        && super::super::github::valid_ref_name(name)
        && number.bytes().all(|b| b.is_ascii_digit())
        && is_oid(sha);
    valid.then(|| QueueBranch {
        base: base.to_owned(),
        pr_number,
        pr_head_sha: sha.to_owned(),
    })
}

/// What a `merge_group` delivery asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupAction {
    ChecksRequested,
    /// GitHub's reason (`merged`, `invalidated` or `dequeued`), or `destroyed` when absent.
    Destroyed(&'static str),
}

/// The group a `merge_group` delivery names. Only identities; nothing here is trusted as state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupEvent {
    pub action: GroupAction,
    pub head_sha: String,
    /// The queue branch, without `refs/heads/`.
    pub head_ref: String,
    pub base_sha: String,
    pub base_ref: String,
    pub queue: QueueBranch,
}

/// Validates a `merge_group` payload. None when an action is unknown, an identity is malformed
/// or the head branch's base disagrees with `base_ref`.
pub fn group_event(payload: &Value) -> Option<GroupEvent> {
    let action = match payload["action"].as_str()? {
        "checks_requested" => GroupAction::ChecksRequested,
        "destroyed" => GroupAction::Destroyed(match payload["reason"].as_str() {
            Some("merged") => "merged",
            Some("invalidated") => "invalidated",
            Some("dequeued") => "dequeued",
            _ => "destroyed",
        }),
        _ => return None,
    };
    let group = &payload["merge_group"];
    let text = |key: &str| group[key].as_str().map(str::to_owned);
    let (head_sha, head_ref) = (text("head_sha")?, text("head_ref")?);
    let (base_sha, base_ref) = (text("base_sha")?, text("base_ref")?);
    let head_ref = head_ref.strip_prefix("refs/heads/")?.to_owned();
    let base_ref = base_ref.strip_prefix("refs/heads/")?.to_owned();
    let queue = queue_branch(&head_ref)?;
    (is_oid(&head_sha) && is_oid(&base_sha) && queue.base == base_ref).then_some(GroupEvent {
        action,
        head_sha,
        head_ref,
        base_sha,
        base_ref,
        queue,
    })
}

/// Facts the Hub read from GitHub (and its own journal) for one group, gathered by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveGroup {
    pub head_sha: String,
    pub base_ref: String,
    pub base_sha: String,
    pub head_tree: String,
    /// The queued pull request: its live state, as for a pull request check.
    pub pr_open: bool,
    /// The head the queue branch names, which GitHub combined into the group.
    pub queued_head_sha: String,
    /// The pull request's live head.
    pub pr_head_sha: String,
    pub pr_head_repo_id: u64,
    pub pr_base_repo_id: u64,
    /// The group is built on another queue group the Hub saw, so it combines other changes.
    pub stacked: bool,
}

/// Whether the group still exists as that commit: the queue branch is the only witness.
pub fn lifecycle(head_sha: &str, branch_tip: Option<&str>) -> Option<&'static str> {
    match branch_tip {
        None => Some("merge group branch is gone"),
        Some(tip) if tip != head_sha => Some("merge group was rebuilt"),
        Some(_) => None,
    }
}

/// The status a merge group gets. Never success unless the group's own tree is the qualified one.
pub fn evaluate_group(
    connected_repo_id: u64,
    subject: &CandidateSubject,
    manifest_commit: &str,
    live: &LiveGroup,
    native: &NativeState,
) -> Verdict {
    match native {
        NativeState::Unavailable(reason) => return Verdict::Error(reason),
        NativeState::Stale(reason) => return Verdict::Failure(reason),
        NativeState::NotQualified | NativeState::Qualified { .. } => {}
    }
    if !live.pr_open {
        return Verdict::Failure("queued pull request is not open");
    }
    if live.pr_head_repo_id != connected_repo_id || live.pr_base_repo_id != connected_repo_id {
        return Verdict::Failure("queued pull request is not within the connected repository");
    }
    if live.base_ref != subject.target {
        return Verdict::Failure("merge group base is not the candidate target");
    }
    let ids = [
        &live.head_sha,
        &live.base_sha,
        &live.head_tree,
        &live.queued_head_sha,
        &live.pr_head_sha,
        manifest_commit,
    ];
    if !ids.into_iter().all(is_oid) {
        return Verdict::Failure("malformed commit identity");
    }
    if live.queued_head_sha != live.pr_head_sha {
        return Verdict::Failure("pull request head changed after it was queued");
    }
    if live.queued_head_sha != manifest_commit {
        return Verdict::Failure("queued head is not the published candidate");
    }
    if live.base_sha != subject.baseline_commit {
        return if live.stacked {
            Verdict::Pending("unsupported: no evidence covers this group's combined source")
        } else {
            Verdict::Failure("target advanced since the candidate baseline")
        };
    }
    match native {
        NativeState::Qualified { tree_oid, .. } if *tree_oid == live.head_tree => Verdict::Success,
        NativeState::Qualified { .. } => {
            Verdict::Failure("merge group tree differs from the qualified revision")
        }
        _ => Verdict::Pending("awaiting native qualification"),
    }
}

/// Domain-separated digest of everything a group success depends on.
pub fn group_binding_digest(
    connected_repo_id: u64,
    pr_number: i64,
    live: &LiveGroup,
    subject: &CandidateSubject,
    native_repository: &str,
    requirement_digest: &str,
    receipt: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm-forge-check-merge-group-binding/v1\0");
    for value in [
        connected_repo_id.to_string(),
        pr_number.to_string(),
        live.head_sha.clone(),
        live.head_tree.clone(),
        live.base_ref.clone(),
        live.base_sha.clone(),
        live.queued_head_sha.clone(),
        native_repository.to_owned(),
        subject.change.id.clone(),
        subject.change_version.to_string(),
        subject.context_digest.clone(),
        subject.policy_epoch.to_string(),
        requirement_digest.to_owned(),
        subject.revision.clone(),
        receipt.to_owned(),
    ] {
        field(&mut hasher, value.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Whether the target's rulesets require the context, and from which source. Read-only; the Hub
/// never changes rulesets or branch protection. Classic branch protection is not read (it needs
/// administration access), so `not_required` means only "no ruleset requires it".
pub fn ruleset_requirement(rules: &Value, context: &str, app_id: Option<i64>) -> &'static str {
    let Some(rules) = rules.as_array() else {
        return "unknown";
    };
    let mut found: Option<&'static str> = None;
    for rule in rules
        .iter()
        .filter(|r| r["type"] == "required_status_checks")
    {
        let Some(checks) = rule["parameters"]["required_status_checks"].as_array() else {
            return "unknown";
        };
        for check in checks.iter().filter(|c| c["context"] == context) {
            let source = match (check["integration_id"].as_i64(), app_id) {
                (None, _) => "required_any_source",
                (Some(id), Some(app)) if id == app => "required_from_this_app",
                (Some(_), _) => "required_from_another_source",
            };
            // The weakest requirement wins: any rule that accepts any source lets others post it.
            found = Some(match (found, source) {
                (Some("required_any_source"), _) | (_, "required_any_source") => {
                    "required_any_source"
                }
                (Some("required_from_another_source"), _) => "required_from_another_source",
                _ => source,
            });
        }
    }
    found.unwrap_or("not_required")
}

/// Whether the newest status of `context` on the group commit is the Hub's own post.
/// `statuses` is one page of GitHub's newest-first listing; `complete` says it was not truncated.
pub fn status_source(
    statuses: &Value,
    complete: bool,
    context: &str,
    expected_login: Option<&str>,
) -> &'static str {
    let Some(expected) = expected_login else {
        return "unknown";
    };
    let newest = statuses
        .as_array()
        .and_then(|list| list.iter().find(|s| s["context"] == context));
    match newest.map(|s| s["creator"]["login"].as_str()) {
        Some(Some(login)) if login.eq_ignore_ascii_case(expected) => "hub",
        Some(Some(_)) => "another_account",
        Some(None) => "unknown",
        None if complete => "absent",
        None => "unknown",
    }
}

/// Current groups rechecked per poll, oldest observation first.
const MAX_GROUPS_PER_PASS: usize = 20;
/// How many queue groups beneath one the projection follows.
const MAX_STACK_DEPTH: usize = 10;

/// The opted-in profile that publishes the context for `base_ref` of `repo`.
fn profile_for(
    state: &HubState,
    repo: &ConnectedRepository,
    base_ref: &str,
) -> Option<AgentTaskProfile> {
    state
        .workspace_execution
        .iter()
        .flat_map(|service| service.agent_task_profiles.values())
        .find(|p| {
            p.publish_forge_status
                && p.repository_id.eq_ignore_ascii_case(&repo.full_name)
                && p.target == base_ref
        })
        .cloned()
}

/// Handles one signature-verified `merge_group` delivery for `repo`; the caller holds the
/// repository lock. The payload only names the group: a destroy ends it (a later request never
/// revives it), and a request is judged from GitHub's current state. A group for a target no
/// opted-in profile publishes is not recorded.
pub(in crate::hub) async fn on_delivery(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    payload: &Value,
) -> Gh<()> {
    if conn.provider != "github" {
        return Ok(());
    }
    let Some(event) = group_event(payload) else {
        eprintln!("hub: unusable merge_group delivery for {}", repo.full_name);
        return Ok(());
    };
    let Some(profile) = profile_for(state, repo, &event.base_ref) else {
        return Ok(());
    };
    let ended = match event.action {
        GroupAction::Destroyed(reason) => Some(reason),
        GroupAction::ChecksRequested => None,
    };
    let new = NewMergeGroup {
        head_sha: event.head_sha,
        head_ref: event.head_ref,
        base_ref: event.base_ref,
        base_sha: event.base_sha,
        pr_number: event.queue.pr_number,
        pr_head_sha: event.queue.pr_head_sha,
        context: status_context(&profile.target)?,
    };
    let (store, id) = (state.store.clone(), repo.id.clone());
    let row = blocking(state, move || {
        store.observe_merge_group(&id, &new, ended, unix_now())
    })
    .await?;
    if row.ended_reason.is_none() {
        reconcile_group(state, conn, repo, token, &row).await?;
    }
    Ok(())
}

/// Rechecks the oldest current groups of `repo`: recovery after a restart, a lost reply, a
/// missed delivery or a changed policy. `Auth` and `Limited` stop the pass.
pub(super) async fn reconcile_active(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
) -> Gh<()> {
    if conn.provider != "github" {
        return Ok(());
    }
    let (store, id) = (state.store.clone(), repo.id.clone());
    let rows = blocking(state, move || {
        store.active_merge_groups(&id, MAX_GROUPS_PER_PASS)
    })
    .await?;
    for row in rows {
        match reconcile_group(state, conn, repo, token, &row).await {
            Ok(()) => {}
            Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => return Err(error),
            Err(GhError::Failed(detail)) => eprintln!(
                "hub: merge group check for {}#{}: {detail}",
                repo.full_name, row.pr_number
            ),
        }
    }
    Ok(())
}

/// What one post on a group should say.
struct Judged<'a> {
    new: NewForgeCheck,
    state: StatusState,
    final_on_refusal: bool,
    authorization: Option<(
        Publication,
        &'a AgentTaskProfile,
        CandidateSubject,
        NativeState,
    )>,
}

async fn end(
    state: &HubState,
    repo: &ConnectedRepository,
    head_sha: &str,
    reason: &'static str,
) -> Result<()> {
    let (store, id, head) = (state.store.clone(), repo.id.clone(), head_sha.to_owned());
    blocking(state, move || {
        store
            .end_merge_group(&id, &head, reason, unix_now())
            .map(drop)
    })
    .await
}

/// One read-decide-post round for one current group.
async fn reconcile_group(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    row: &MergeGroup,
) -> Gh<()> {
    let ForgeApi::Github(api) = state.sync.forge(conn)? else {
        return Ok(());
    };
    let tip = api.ref_sha(token, &repo.full_name, &row.head_ref).await?;
    if let Some(reason) = lifecycle(&row.head_sha, tip.as_deref()) {
        return Ok(end(state, repo, &row.head_sha, reason).await?);
    }
    let profile = profile_for(state, repo, &row.base_ref);
    let judged = match &profile {
        Some(profile) => judge(state, api, repo, token, profile, row).await?,
        // Turned off while queued: retire a success that may show, post nothing otherwise.
        None if row.check.as_ref().is_some_and(may_show_success) => Some(Judged {
            new: NewForgeCheck {
                head_sha: row.head_sha.clone(),
                state: "failure".into(),
                binding_digest: None,
                description: DISABLED.into(),
            },
            state: StatusState::Failure,
            final_on_refusal: true,
            authorization: None,
        }),
        None => None,
    };
    let Some(judged) = judged else {
        return Ok(());
    };
    // A settled post of the same status is not repeated; a refusal waits for a change.
    let settled = row.check.as_ref().is_some_and(|c| {
        matches!(c.phase.as_str(), "posted" | "refused")
            && c.state == judged.new.state
            && c.binding_digest == judged.new.binding_digest
            && c.description == judged.new.description
    });
    if !settled {
        post_group(state, api, repo, token, row, &judged).await?;
    }
    if profile.is_some() {
        observe_source(state, api, conn, repo, token, row).await?;
    }
    Ok(())
}

/// Judges a group from GitHub and the pinned store. None when there is nothing to say: no
/// candidate publication for the queued pull request (as for pull request checks), or the pull
/// request is already merged (its result is history and is left alone).
async fn judge<'a>(
    state: &HubState,
    api: &github::Api,
    repo: &ConnectedRepository,
    token: &str,
    profile: &'a AgentTaskProfile,
    row: &MergeGroup,
) -> Gh<Option<Judged<'a>>> {
    let Some(publication) = newest_publication(state, repo, row.pr_number).await? else {
        return Ok(None);
    };
    let Some(candidate) = publication.manifest().ok().and_then(|m| m.candidate) else {
        return Ok(None);
    };
    if candidate.subject.target != profile.target {
        return Ok(None);
    }
    let Some(pull) = api
        .pull_head(token, &repo.full_name, row.pr_number, &mut HashMap::new())
        .await?
    else {
        return Ok(None);
    };
    if pull.merged {
        return Ok(None);
    }
    let (head_tree, _) = api
        .commit_parts(token, &repo.full_name, &row.head_sha)
        .await?
        .ok_or_else(|| GhError::Failed("merge group commit not found".into()))?;
    let (store, id, base) = (state.store.clone(), repo.id.clone(), row.base_sha.clone());
    let stacked = blocking(state, move || store.merge_group(&id, &base))
        .await?
        .is_some();
    let connected = u64::try_from(repo.repository_id).unwrap_or(0);
    let live = LiveGroup {
        head_sha: row.head_sha.clone(),
        base_ref: row.base_ref.clone(),
        base_sha: row.base_sha.clone(),
        head_tree,
        pr_open: pull.open,
        queued_head_sha: row.pr_head_sha.clone(),
        pr_head_sha: pull.head_sha,
        pr_head_repo_id: pull.head_repo_id,
        pr_base_repo_id: pull.base_repo_id,
        stacked,
    };
    let subject = candidate.subject;
    let native = publication_native(state, &publication, profile, &subject).await?;
    let verdict = evaluate_group(connected, &subject, &publication.commit, &live, &native);
    let (status, description, digest) = match (verdict, &native) {
        (
            Verdict::Success,
            NativeState::Qualified {
                receipt,
                requirement_digest,
                ..
            },
        ) => {
            let digest = group_binding_digest(
                connected,
                row.pr_number,
                &live,
                &subject,
                &profile.native_repository,
                requirement_digest,
                receipt,
            );
            let description = format!(
                "Merge group qualified in the pinned native store; binding {}",
                &digest[..12]
            );
            (StatusState::Success, description, Some(digest))
        }
        (Verdict::Pending(reason), _) => (StatusState::Pending, reason.to_owned(), None),
        (Verdict::Failure(reason), _) => (StatusState::Failure, reason.to_owned(), None),
        (Verdict::Error(reason), _) => (StatusState::Error, reason.to_owned(), None),
        (Verdict::Success, _) => (StatusState::Error, "internal inconsistency".into(), None),
    };
    Ok(Some(Judged {
        new: NewForgeCheck {
            head_sha: row.head_sha.clone(),
            state: status.as_str().into(),
            binding_digest: digest,
            description,
        },
        state: status,
        final_on_refusal: false,
        authorization: Some((publication, profile, subject, native)),
    }))
}

/// Reserves, posts and completes one status on the group commit. The reservation succeeds only
/// for a current group at the generation this pass read, so an ended group or a concurrent
/// writer posts nothing. Before a success the native qualification and the queue branch are
/// read again. A lost reply leaves the row `attempting` and the next pass posts the then-desired
/// status (the newest status of a context wins on GitHub). A definite refusal is `refused`,
/// except when it would leave a success showing.
async fn post_group(
    state: &HubState,
    api: &github::Api,
    repo: &ConnectedRepository,
    token: &str,
    row: &MergeGroup,
    judged: &Judged<'_>,
) -> Gh<()> {
    let status = CommitStatus::new(
        &row.head_sha,
        judged.state,
        &row.context,
        &judged.new.description,
    )?;
    let (store, id, head) = (state.store.clone(), repo.id.clone(), row.head_sha.clone());
    let (expected, new) = (row.generation, judged.new.clone());
    let reserved = blocking(state, move || {
        store.reserve_merge_group_check(&id, &head, expected, &new, unix_now())
    })
    .await?;
    let ForgeCheckReserve::Reserved(generation) = reserved else {
        return Ok(());
    };
    if judged.state == StatusState::Success {
        let authorization =
            judged
                .authorization
                .as_ref()
                .map(
                    |(publication, profile, subject, native)| StatusAuthorization {
                        publication,
                        profile,
                        subject,
                        native,
                    },
                );
        if !status_authorized(state, authorization.as_ref()).await? {
            return Ok(());
        }
        let tip = api.ref_sha(token, &repo.full_name, &row.head_ref).await?;
        if let Some(reason) = lifecycle(&row.head_sha, tip.as_deref()) {
            return Ok(end(state, repo, &row.head_sha, reason).await?);
        }
    }
    let phase = match api
        .post_commit_status(token, &repo.full_name, &status)
        .await
    {
        Ok(Ok(())) => "posted",
        Ok(Err(refused)) => {
            eprintln!(
                "hub: {} refused a merge group check ({refused})",
                repo.full_name
            );
            if !judged.final_on_refusal && row.check.as_ref().is_some_and(may_show_success) {
                return Ok(());
            }
            "refused"
        }
        Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => return Err(error),
        Err(GhError::Failed(detail)) => {
            eprintln!(
                "hub: merge group check post for {}#{}: {detail}",
                repo.full_name, row.pr_number
            );
            return Ok(());
        }
    };
    let (store, id, head) = (state.store.clone(), repo.id.clone(), row.head_sha.clone());
    blocking(state, move || {
        store.complete_merge_group_check(&id, &head, generation, phase, unix_now())
    })
    .await?;
    Ok(())
}

/// Records whether the target's rulesets require the context and from which source, and who
/// posted the newest status of the context on the group. Observation only.
async fn observe_source(
    state: &HubState,
    api: &github::Api,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    row: &MergeGroup,
) -> Gh<()> {
    let requirement = match api
        .branch_rules(token, &repo.full_name, &row.base_ref)
        .await?
    {
        Some(rules) => ruleset_requirement(&rules, &row.context, conn.app_id),
        None => "unknown",
    };
    let expected = match conn.kind.as_str() {
        "app" => conn.app_slug.as_ref().map(|slug| format!("{slug}[bot]")),
        _ => conn.acts_as.clone(),
    };
    let source = match api
        .status_page(token, &repo.full_name, &row.head_sha)
        .await?
    {
        Some((list, complete)) => status_source(&list, complete, &row.context, expected.as_deref()),
        None => "unknown",
    };
    let (store, id, head) = (state.store.clone(), repo.id.clone(), row.head_sha.clone());
    blocking(state, move || {
        store.note_merge_group_source(&id, &head, requirement, source)
    })
    .await?;
    Ok(())
}

/// The projection of `repo`'s group `head_sha`, following the queue groups beneath it.
pub async fn group_projection(
    state: &HubState,
    repo_id: &str,
    head_sha: &str,
) -> Result<Option<Projection>> {
    let (store, id, head) = (state.store.clone(), repo_id.to_owned(), head_sha.to_owned());
    blocking(state, move || {
        let Some(row) = store.merge_group(&id, &head)? else {
            return Ok(None);
        };
        let mut stacked_on = Vec::new();
        let mut base = row.base_sha.clone();
        while stacked_on.len() < MAX_STACK_DEPTH {
            let Some(below) = store.merge_group(&id, &base)? else {
                break;
            };
            stacked_on.push(below.pr_number);
            base = below.base_sha;
        }
        Ok(Some(projection(&row, stacked_on)))
    })
    .await
}

pub fn projection(row: &MergeGroup, stacked_on: Vec<i64>) -> Projection {
    let check = row.check.as_ref();
    Projection {
        kind: "merge_group",
        context: row.context.clone(),
        group_sha: row.head_sha.clone(),
        base_ref: row.base_ref.clone(),
        base_sha: row.base_sha.clone(),
        pr_number: row.pr_number,
        pr_head_sha: row.pr_head_sha.clone(),
        stacked_on,
        lifecycle: if row.ended_reason.is_some() {
            "ended"
        } else {
            "current"
        },
        ended_reason: row.ended_reason.clone(),
        state: check.map(|c| c.state.clone()),
        description: check.map(|c| c.description.clone()),
        binding_digest: check.and_then(|c| c.binding_digest.clone()),
        posting: match check.map(|c| c.phase.as_str()) {
            None => "pending",
            Some("posted") => "posted",
            Some("refused") => "refused",
            Some(_) => "uncertain",
        },
        ruleset_requirement: row
            .ruleset_requirement
            .clone()
            .unwrap_or_else(|| "unknown".into()),
        status_source: row
            .status_source
            .clone()
            .unwrap_or_else(|| "unknown".into()),
        observed_at: row.observed_at,
        updated_at: check.map_or(row.observed_at, |c| c.updated_at),
    }
}

/// The compact, read-only view consumers use instead of scraping a status description.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Projection {
    pub kind: &'static str,
    pub context: String,
    pub group_sha: String,
    pub base_ref: String,
    pub base_sha: String,
    pub pr_number: i64,
    pub pr_head_sha: String,
    /// Queue groups the Hub saw directly beneath this one, nearest first (pull request numbers).
    pub stacked_on: Vec<i64>,
    pub lifecycle: &'static str,
    pub ended_reason: Option<String>,
    pub state: Option<String>,
    pub description: Option<String>,
    pub binding_digest: Option<String>,
    /// `pending` (nothing posted), `uncertain`, `posted` or `refused`.
    pub posting: &'static str,
    pub ruleset_requirement: String,
    pub status_source: String,
    pub observed_at: i64,
    pub updated_at: i64,
}

#[cfg(test)]
mod tests;
