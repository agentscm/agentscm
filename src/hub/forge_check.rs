//! Pure decision for a Hub-published forge commit status on a candidate pull request.
//!
//! Success needs the Hub's own read of an operator-pinned native store and of the live pull
//! request head. Webhook or comment text is never an input here. Nothing in this module
//! performs network I/O or writes state.
use sha2::{Digest, Sha256};

use super::connection::{
    ConnectedRepository, Connection, ForgeCheck, ForgeCheckReserve, NewForgeCheck,
};
use super::github::{CommitStatus, Gh, GhError, StatusState};
use super::http::blocking;
use super::store::unix_now;
use super::workspace_execution::AgentTaskProfile;
use super::{HubError, HubState, Result};
use crate::authority::workflows::CurrentEvidence;
use crate::candidate::CandidateSubject;
use crate::skill_workflow::LiveImportedOrigins;

/// What the Hub itself verified in the native store for one candidate subject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeState {
    Qualified {
        receipt: String,
        requirement_digest: String,
        tree_oid: String,
    },
    NotQualified,
    Stale(&'static str),
    Unavailable(&'static str),
}

/// Facts observed from the forge for one pull request, gathered by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiveHead {
    pub open: bool,
    pub head_sha: String,
    pub head_repo_id: u64,
    pub base_repo_id: u64,
    pub base_ref: String,
    /// Live tip of the base ref, not the pull request's recorded base commit.
    pub base_tip: String,
    pub head_tree: String,
    pub head_parents: Vec<String>,
}

/// One-to-one with the forge status states. Reasons are static and never echo forge text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Success,
    Pending(&'static str),
    Failure(&'static str),
    Error(&'static str),
}

pub fn native_state(profile: &AgentTaskProfile, subject: &CandidateSubject) -> NativeState {
    let Ok(authority) = profile.stable_mapping() else {
        return NativeState::Unavailable("native store unavailable");
    };
    let requirement = match authority.workflow_requirement(&profile.target) {
        Ok(Some(requirement)) => requirement,
        Ok(None) => return NativeState::NotQualified,
        Err(_) => return NativeState::Unavailable("native requirement unreadable"),
    };
    match requirement.digest() {
        Ok(digest) if digest == profile.requirement_digest => {}
        Ok(_) => return NativeState::Stale("requirement differs from pin"),
        Err(_) => return NativeState::Unavailable("native requirement unreadable"),
    }
    if subject.target != profile.target {
        return NativeState::Stale("candidate target differs from profile");
    }
    let Ok(context) = authority.review_context(&subject.context_digest) else {
        return NativeState::Unavailable("candidate context unreadable");
    };
    if context.change != subject.change
        || context.change.repository != profile.native_repository
        || context.change_version != subject.change_version
        || context.revision != subject.revision
        || context.target != subject.target
        || context.policy_epoch != subject.policy_epoch
    {
        return NativeState::Stale("candidate context differs");
    }
    match authority.change(&subject.change) {
        Ok(change)
            if change.version == subject.change_version
                && change.context_digest == subject.context_digest => {}
        Ok(_) => return NativeState::Stale("change moved"),
        Err(_) => return NativeState::Unavailable("native change unreadable"),
    }
    let origins = LiveImportedOrigins::native_only(profile.native_repository.clone());
    let receipt = match authority.current_qualification(&context, &origins) {
        Ok(CurrentEvidence::Receipt(receipt)) => receipt,
        Ok(CurrentEvidence::NotQualified) => return NativeState::NotQualified,
        Ok(CurrentEvidence::SubjectChanged) => {
            return NativeState::Stale("candidate subject changed");
        }
        Err(_) => return NativeState::Unavailable("qualification unreadable"),
    };
    let tree_oid = crate::store::Engine::open_existing(&profile.data_dir)
        .and_then(|engine| crate::git_export::native_tree_oid(&engine, &subject.revision));
    match tree_oid {
        Ok(tree_oid) => NativeState::Qualified {
            receipt,
            requirement_digest: profile.requirement_digest.clone(),
            tree_oid,
        },
        Err(_) => NativeState::Unavailable("revision tree unavailable"),
    }
}

fn is_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Candidate commits keep the prior output as first parent and add the target baseline as a
/// second parent only when it is a different commit.
fn expected_parents(subject: &CandidateSubject) -> Vec<&str> {
    let mut parents = vec![subject.parent_commit.as_str()];
    if subject.baseline_commit != subject.parent_commit {
        parents.push(subject.baseline_commit.as_str());
    }
    parents
}

pub fn evaluate(
    connected_repo_id: u64,
    subject: &CandidateSubject,
    manifest_commit: &str,
    live: &LiveHead,
    native: &NativeState,
) -> Verdict {
    match native {
        NativeState::Unavailable(reason) => return Verdict::Error(reason),
        NativeState::Stale(reason) => return Verdict::Failure(reason),
        NativeState::NotQualified | NativeState::Qualified { .. } => {}
    }
    if !live.open {
        return Verdict::Failure("pull request is not open");
    }
    if live.head_repo_id != connected_repo_id || live.base_repo_id != connected_repo_id {
        return Verdict::Failure("pull request is not within the connected repository");
    }
    if live.base_ref != subject.target {
        return Verdict::Failure("pull request base is not the candidate target");
    }
    let ids = [
        &live.head_sha,
        &live.base_tip,
        &live.head_tree,
        manifest_commit,
    ];
    if !ids.into_iter().all(is_oid) {
        return Verdict::Failure("malformed commit identity");
    }
    if live.base_tip != subject.baseline_commit {
        return Verdict::Failure("target advanced since the candidate baseline");
    }
    if live.head_sha != manifest_commit {
        return Verdict::Failure("pull request head is not the published candidate");
    }
    if live
        .head_parents
        .iter()
        .map(String::as_str)
        .ne(expected_parents(subject))
    {
        return Verdict::Failure("candidate commit parents differ");
    }
    match native {
        NativeState::Qualified { tree_oid, .. } if *tree_oid == live.head_tree => Verdict::Success,
        NativeState::Qualified { .. } => Verdict::Failure("head tree differs from native revision"),
        _ => Verdict::Pending("awaiting native qualification"),
    }
}

fn field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

/// Domain-separated digest of everything a posted status depends on.
pub fn binding_digest(
    connected_repo_id: u64,
    pr_number: u64,
    live: &LiveHead,
    subject: &CandidateSubject,
    native_repository: &str,
    requirement_digest: &str,
    receipt: &str,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"agentscm-forge-check-binding/v1\0");
    for value in [
        connected_repo_id.to_string(),
        pr_number.to_string(),
        live.head_sha.clone(),
        live.base_ref.clone(),
        live.base_tip.clone(),
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

/// `agentscm/workflow/{target}` for a conservative target spelling.
pub fn status_context(target: &str) -> Result<String> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/');
    if target.is_empty()
        || target.len() > 100
        || !target.chars().all(allowed)
        || !super::github::valid_ref_name(target)
    {
        return Err(HubError::Invalid("invalid status target".into()));
    }
    Ok(format!("agentscm/workflow/{target}"))
}

/// Pull requests examined per pass; the rest wait for the next one.
const MAX_PULLS_PER_PASS: usize = 50;
const SUPERSEDED: &str = "superseded: pull request head changed";

/// What one status post should say about one head.
struct Desired {
    new: NewForgeCheck,
    state: StatusState,
}

/// Re-reads every candidate pull request of `repo` from the forge and the pinned native store
/// and posts the status each opted-in profile calls for. The caller holds the repository lock.
/// `Auth` and `Limited` stop the pass; anything else skips only the pull request concerned.
pub(super) async fn reconcile_repo(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
) -> Gh<()> {
    // Statuses are a GitHub feature; another forge's token never goes to the GitHub client.
    let Some(service) = state
        .workspace_execution
        .as_ref()
        .filter(|_| conn.provider == "github")
    else {
        return Ok(());
    };
    let profiles: Vec<AgentTaskProfile> = service
        .agent_task_profiles
        .values()
        .filter(|p| p.publish_forge_status && p.repository_id.eq_ignore_ascii_case(&repo.full_name))
        .cloned()
        .collect();
    if profiles.is_empty() {
        return Ok(());
    }
    let (store, name) = (state.store.clone(), repo.full_name.clone());
    let publications = blocking(state, move || {
        store.pull_publications(&name, MAX_PULLS_PER_PASS)
    })
    .await?;
    let mut last = 0;
    for publication in publications {
        // Newest publication of a pull request first; older ones of the same number are history.
        let Some(number) = publication.pr_number.filter(|n| *n != last) else {
            continue;
        };
        last = number;
        let Some(candidate) = publication.manifest().ok().and_then(|m| m.candidate) else {
            continue;
        };
        for profile in profiles
            .iter()
            .filter(|p| p.target == candidate.subject.target)
        {
            // Two passes at most: the second is the re-read after a success was posted.
            for _ in 0..2 {
                let step = pull_step(
                    state,
                    repo,
                    token,
                    profile,
                    number,
                    &candidate.subject,
                    &publication.commit,
                )
                .await;
                match step {
                    Ok(true) => {}
                    Ok(false) => break,
                    Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => return Err(error),
                    Err(GhError::Failed(detail)) => {
                        eprintln!("hub: forge check for {}#{number}: {detail}", repo.full_name);
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

/// One read-decide-post round for one pull request. True when it posted a success, so the
/// caller reads again before trusting it.
async fn pull_step(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    profile: &AgentTaskProfile,
    number: i64,
    subject: &CandidateSubject,
    commit: &str,
) -> Gh<bool> {
    let api = state.sync.api()?;
    // A closed or missing pull request gets no new post; a read that fails claims nothing.
    let Some(head) = api.pull_head(token, &repo.full_name, number).await? else {
        return Ok(false);
    };
    if !head.open {
        return Ok(false);
    }
    let live = LiveHead {
        open: head.open,
        head_sha: head.head_sha,
        head_repo_id: head.head_repo_id,
        base_repo_id: head.base_repo_id,
        base_ref: head.base_ref,
        base_tip: head.base_tip,
        head_tree: head.head_tree,
        head_parents: head.head_parents,
    };
    let (task, task_subject) = (profile.clone(), subject.clone());
    let native = blocking(state, move || Ok(native_state(&task, &task_subject))).await?;
    let connected = u64::try_from(repo.repository_id).unwrap_or(0);
    let verdict = evaluate(connected, subject, commit, &live, &native);
    let desired = desired(connected, number, profile, subject, &live, &native, verdict);

    let context = status_context(&profile.target)?;
    let row = journal(state, repo, number, &context).await?;
    if let Some(old) = row
        .as_ref()
        .filter(|r| r.state == "success" && r.head_sha != live.head_sha)
    {
        // A success must not stay on a commit the pull request may return to.
        let stale = Desired {
            new: NewForgeCheck {
                head_sha: old.head_sha.clone(),
                state: "failure".into(),
                binding_digest: None,
                description: SUPERSEDED.into(),
            },
            state: StatusState::Failure,
        };
        if !post(state, repo, token, number, &context, row.as_ref(), &stale).await? {
            return Ok(false);
        }
    }
    let row = journal(state, repo, number, &context).await?;
    if row.as_ref().is_some_and(|r| {
        r.phase == "posted"
            && r.head_sha == desired.new.head_sha
            && r.state == desired.new.state
            && r.binding_digest == desired.new.binding_digest
            && r.description == desired.new.description
    }) {
        return Ok(false);
    }
    let posted = post(state, repo, token, number, &context, row.as_ref(), &desired).await?;
    Ok(posted && desired.state == StatusState::Success)
}

fn desired(
    connected: u64,
    number: i64,
    profile: &AgentTaskProfile,
    subject: &CandidateSubject,
    live: &LiveHead,
    native: &NativeState,
    verdict: Verdict,
) -> Desired {
    let (state, description, digest) = match (verdict, native) {
        (
            Verdict::Success,
            NativeState::Qualified {
                receipt,
                requirement_digest,
                ..
            },
        ) => {
            let digest = binding_digest(
                connected,
                number as u64,
                live,
                subject,
                &profile.native_repository,
                requirement_digest,
                receipt,
            );
            (
                StatusState::Success,
                format!(
                    "Qualified in the pinned native store; binding {}",
                    &digest[..12]
                ),
                Some(digest),
            )
        }
        (Verdict::Pending(reason), _) => (StatusState::Pending, reason.to_owned(), None),
        (Verdict::Failure(reason), _) => (StatusState::Failure, reason.to_owned(), None),
        // Success without a qualified native state cannot be produced by `evaluate`; never post it.
        (Verdict::Error(reason), _) => (StatusState::Error, reason.to_owned(), None),
        (Verdict::Success, _) => (StatusState::Error, "internal inconsistency".into(), None),
    };
    Desired {
        new: NewForgeCheck {
            head_sha: live.head_sha.clone(),
            state: state.as_str().into(),
            binding_digest: digest,
            description,
        },
        state,
    }
}

async fn journal(
    state: &HubState,
    repo: &ConnectedRepository,
    number: i64,
    context: &str,
) -> Result<Option<ForgeCheck>> {
    let (store, id, context) = (state.store.clone(), repo.id.clone(), context.to_owned());
    blocking(state, move || store.forge_check(&id, number, &context)).await
}

/// Reserves, posts and completes one status. True only when the forge accepted it and the
/// journal says `posted`. A lost reply, a definite refusal or a lost race leaves the row
/// `attempting` (or unchanged), so the next pass posts the desired status again; the latest
/// status per context wins on GitHub, so the repost is the recovery. A refusal is never
/// recorded as posted.
async fn post(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    number: i64,
    context: &str,
    row: Option<&ForgeCheck>,
    desired: &Desired,
) -> Gh<bool> {
    let status = CommitStatus::new(
        &desired.new.head_sha,
        desired.state,
        context,
        &desired.new.description,
    )?;
    let (store, id, ctx) = (state.store.clone(), repo.id.clone(), context.to_owned());
    let (expected, new) = (row.map(|r| r.generation), desired.new.clone());
    let reserved = blocking(state, move || {
        store.reserve_forge_check(&id, number, &ctx, expected, &new, unix_now())
    })
    .await?;
    let ForgeCheckReserve::Reserved(generation) = reserved else {
        return Ok(false);
    };
    match state
        .sync
        .api()?
        .post_commit_status(token, &repo.full_name, &status)
        .await
    {
        Ok(Ok(())) => {}
        Ok(Err(refused)) => {
            eprintln!("hub: {} refused a forge check ({refused})", repo.full_name);
            return Ok(false);
        }
        // Not applied; the row stays `attempting`.
        Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => return Err(error),
        // Uncertain: the status may exist.
        Err(GhError::Failed(detail)) => {
            eprintln!(
                "hub: forge check post for {}#{number}: {detail}",
                repo.full_name
            );
            return Ok(false);
        }
    }
    let (store, id, ctx) = (state.store.clone(), repo.id.clone(), context.to_owned());
    Ok(blocking(state, move || {
        store.complete_forge_check(&id, number, &ctx, generation, unix_now())
    })
    .await?)
}

#[cfg(test)]
mod tests;
