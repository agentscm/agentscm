//! Pure decision for a Hub-published forge commit status on a candidate pull request.
//!
//! Success needs the Hub's own read of an operator-pinned native store and of the live pull
//! request head. Webhook or comment text is never an input here. Nothing in this module
//! performs network I/O or writes state.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};

use super::connection::{
    ConnectedRepository, Connection, ForgeCheck, ForgeCheckReserve, NewForgeCheck,
};
use super::github::{CommitStatus, Gh, GhError, StatusState};
use super::http::blocking;
use super::publish::Update;
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
    match cached_tree_oid(&profile.data_dir, &subject.revision) {
        Ok(tree_oid) => NativeState::Qualified {
            receipt,
            requirement_digest: profile.requirement_digest.clone(),
            tree_oid,
        },
        Err(_) => NativeState::Unavailable("revision tree unavailable"),
    }
}

/// A revision is immutable, so its tree OID is computed once per store.
/// ponytail: cleared whole past 256 entries; an LRU only matters beyond that many live revisions.
fn cached_tree_oid(data_dir: &std::path::Path, revision: &str) -> crate::Result<String> {
    static TREES: Mutex<Option<HashMap<(PathBuf, String), String>>> = Mutex::new(None);
    let key = (data_dir.to_path_buf(), revision.to_owned());
    if let Some(found) = TREES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(HashMap::new)
        .get(&key)
    {
        return Ok(found.clone());
    }
    let engine = crate::store::Engine::open_existing(data_dir)?;
    let oid = crate::git_export::native_tree_oid(&engine, revision)?;
    let mut trees = TREES.lock().unwrap_or_else(PoisonError::into_inner);
    let trees = trees.get_or_insert_with(HashMap::new);
    if trees.len() >= 256 {
        trees.clear();
    }
    trees.insert(key, oid.clone());
    Ok(oid)
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
const CLOSED: &str = "not applicable: pull request closed";
const DISABLED: &str = "not applicable: forge check disabled";

/// How one status post ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Posting {
    /// The forge accepted it and the journal says `posted`.
    Posted,
    /// The forge accepted it but the journal write lost a race; read again.
    Unrecorded,
    /// The forge definitely refused it; recorded as `refused`, unless a success may remain.
    Refused,
    /// Not applied, uncertain, or not reserved: the row is left for the next pass.
    Unknown,
}

/// A status on `row` may still show a success: it says success, or its last post is uncertain.
fn may_show_success(row: &ForgeCheck) -> bool {
    row.state == "success" || row.phase == "attempting"
}

/// What one status post should say about one head.
struct Desired {
    new: NewForgeCheck,
    state: StatusState,
    /// A refusal settles the row even when the row it replaces may show a success. Only a
    /// retire sets this: it moves on rather than retry, so a forbidden commit cannot wedge it.
    final_on_refusal: bool,
}

/// Re-reads every candidate pull request of `repo` from the forge and the pinned native store
/// and posts the status each opted-in profile calls for. The caller holds the repository lock.
/// `Auth` and `Limited` stop the pass; anything else skips only the pull request concerned.
/// `only` limits the pass to one pull request; otherwise each pass takes the next window of
/// `MAX_PULLS_PER_PASS` pull requests after the previous one's last, wrapping at the end.
pub(super) async fn reconcile_repo(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
    only: Option<i64>,
) -> Gh<()> {
    // Statuses are a GitHub feature; another forge's token never goes to the GitHub client.
    if conn.provider != "github" {
        return Ok(());
    }
    let profiles: Vec<AgentTaskProfile> = state
        .workspace_execution
        .iter()
        .flat_map(|service| service.agent_task_profiles.values())
        .filter(|p| p.publish_forge_status && p.repository_id.eq_ignore_ascii_case(&repo.full_name))
        .cloned()
        .collect();
    if only.is_none() {
        retire_disabled(state, repo, token, &profiles).await?;
    }
    if profiles.is_empty() {
        return Ok(());
    }
    let (after, limit) = match only {
        Some(number) => (number.saturating_sub(1), 1),
        None => (state.sync.forge_cursor(&repo.id), MAX_PULLS_PER_PASS),
    };
    let (store, name) = (state.store.clone(), repo.full_name.clone());
    let publications =
        blocking(state, move || store.pull_publications(&name, after, limit)).await?;
    let full = publications.len() >= limit;
    let mut base_tips = HashMap::new();
    let last = publications.last().and_then(|p| p.pr_number).unwrap_or(0);
    for publication in publications {
        // One (newest) publication per pull request number; older ones are history.
        let Some(number) = publication
            .pr_number
            .filter(|n| only.is_none_or(|o| o == *n))
        else {
            continue;
        };
        let Some(candidate) = publication.manifest().ok().and_then(|m| m.candidate) else {
            continue;
        };
        for profile in profiles
            .iter()
            .filter(|p| p.target == candidate.subject.target)
        {
            // Two passes at most: the second is the re-read after a success was posted.
            for attempt in 0..2 {
                if attempt == 1 {
                    // The confirming read judges the base as it is now, and later pull
                    // requests in this pass reuse that fresh tip.
                    base_tips.clear();
                }
                let step = pull_step(
                    state,
                    repo,
                    token,
                    profile,
                    number,
                    (&candidate.subject, &publication.commit),
                    &mut base_tips,
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
    if only.is_none() {
        state
            .sync
            .set_forge_cursor(&repo.id, if full { last } else { 0 });
    }
    Ok(())
}

/// Retires a success (or uncertain post) on a context no enabled profile of `repo` publishes
/// any more: the flag was turned off, the profile removed or its target changed. The store
/// leaves out enabled contexts and merges the Hub recorded, so neither crowds out the rest; a
/// pull request the forge reports merged is recorded as merged and one that cannot be read is
/// skipped this pass; both leave the row. At most `MAX_PULLS_PER_PASS` rows per pass; a
/// retired row no longer qualifies, so none repeats.
async fn retire_disabled(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    profiles: &[AgentTaskProfile],
) -> Gh<()> {
    let enabled: Vec<String> = profiles
        .iter()
        .filter_map(|p| status_context(&p.target).ok())
        .collect();
    let (store, id, name) = (state.store.clone(), repo.id.clone(), repo.full_name.clone());
    let rows = blocking(state, move || {
        store.forge_checks_showing_success(&id, &name, &enabled, MAX_PULLS_PER_PASS)
    })
    .await?;
    let api = state.sync.api()?;
    for (number, context, row) in rows {
        // The store knows only merges the Hub recorded; ask the forge, and skip on a failed read.
        match api.pull(token, &repo.full_name, number).await {
            Ok(Some(pull)) => match pull["merged"].as_bool() {
                Some(false) => {}
                Some(true) => {
                    let merge_commit = pull["merge_commit_sha"].as_str().map(str::to_owned);
                    record_merge(state, repo, number, merge_commit).await?;
                    continue;
                }
                None => {
                    eprintln!(
                        "hub: forge check for {}#{number}: unreadable reply from pulls",
                        repo.full_name
                    );
                    continue;
                }
            },
            Ok(None) => {}
            Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => return Err(error),
            Err(GhError::Failed(detail)) => {
                eprintln!("hub: forge check for {}#{number}: {detail}", repo.full_name);
                continue;
            }
        }
        retire(state, repo, token, number, &context, &row, DISABLED).await?;
    }
    Ok(())
}

/// Records a merge the forge reported on the pull request's newest publication, as the
/// publication read does. A Hub-store write only. A merge the forge names no commit for is not
/// recorded, since the publication needs one.
async fn record_merge(
    state: &HubState,
    repo: &ConnectedRepository,
    number: i64,
    merge_commit: Option<String>,
) -> Result<()> {
    let Some(merge_commit) = merge_commit else {
        return Ok(());
    };
    let (store, name) = (state.store.clone(), repo.full_name.clone());
    blocking(state, move || {
        // The listing omits merged pull requests, so a match is the unrecorded newest one.
        let newest = store.pull_publications(&name, number.saturating_sub(1), 1)?;
        if let Some(publication) = newest.iter().find(|p| p.pr_number == Some(number)) {
            store.update_publication(
                &publication.id,
                &Update::Merged { merge_commit },
                unix_now(),
            )?;
        }
        Ok(())
    })
    .await
}

/// One read-decide-post round for one pull request. True when it posted a success, so the
/// caller reads again; if that confirming read fails, the posted success stays for the next
/// pass to recheck.
async fn pull_step(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    profile: &AgentTaskProfile,
    number: i64,
    (subject, commit): (&CandidateSubject, &str),
    base_tips: &mut HashMap<String, String>,
) -> Gh<bool> {
    let api = state.sync.api()?;
    // A missing pull request gets no post; a read that fails claims nothing.
    let Some(head) = api
        .pull_head(token, &repo.full_name, number, base_tips)
        .await?
    else {
        return Ok(false);
    };
    // A merged head is history, not a candidate: leave whatever it shows alone, and record the
    // merge so later passes skip this pull request.
    if head.merged {
        record_merge(state, repo, number, head.merge_commit).await?;
        return Ok(false);
    }
    let context = status_context(&profile.target)?;
    let row = journal(state, repo, number, &context).await?;
    if !head.open {
        // A success must not stay on a commit once its pull request is closed.
        if let Some(old) = row.as_ref().filter(|r| may_show_success(r)) {
            retire(state, repo, token, number, &context, old, CLOSED).await?;
        }
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

    if let Some(old) = row
        .as_ref()
        .filter(|r| r.head_sha != live.head_sha && may_show_success(r))
    {
        // A success must not stay on a commit the pull request may return to. Only a settled
        // failure (posted or refused) lets the row move to the new head.
        let settled = retire(state, repo, token, number, &context, old, SUPERSEDED).await?;
        if !settled {
            return Ok(false);
        }
    }
    let row = journal(state, repo, number, &context).await?;
    // A refusal is not retried until the desired status changes.
    if row.as_ref().is_some_and(|r| {
        matches!(r.phase.as_str(), "posted" | "refused")
            && r.head_sha == desired.new.head_sha
            && r.state == desired.new.state
            && r.binding_digest == desired.new.binding_digest
            && r.description == desired.new.description
    }) {
        return Ok(false);
    }
    let posted = post(state, repo, token, number, &context, row.as_ref(), &desired).await?;
    Ok(matches!(posted, Posting::Posted | Posting::Unrecorded)
        && desired.state == StatusState::Success)
}

/// Posts a failure on the head `old` names. True when that failure is settled (the forge
/// accepted it or definitely refused it), so a gc'd or forbidden commit cannot wedge the pull
/// request. A lost or unrecorded post leaves the row for the next pass.
async fn retire(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    number: i64,
    context: &str,
    old: &ForgeCheck,
    description: &str,
) -> Gh<bool> {
    let stale = Desired {
        new: NewForgeCheck {
            head_sha: old.head_sha.clone(),
            state: "failure".into(),
            binding_digest: None,
            description: description.into(),
        },
        state: StatusState::Failure,
        final_on_refusal: true,
    };
    let posting = post(state, repo, token, number, context, Some(old), &stale).await?;
    Ok(matches!(posting, Posting::Posted | Posting::Refused))
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
        final_on_refusal: false,
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

/// Reserves, posts and completes one status. A lost reply or a lost race leaves the row
/// `attempting` (or unchanged), so the next pass posts the desired status again; the latest
/// status per context wins on GitHub, so the repost is the recovery. A definite refusal is
/// recorded as `refused` and never as posted.
async fn post(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    number: i64,
    context: &str,
    row: Option<&ForgeCheck>,
    desired: &Desired,
) -> Gh<Posting> {
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
        return Ok(Posting::Unknown);
    };
    let phase = match state
        .sync
        .api()?
        .post_commit_status(token, &repo.full_name, &status)
        .await
    {
        Ok(Ok(())) => "posted",
        Ok(Err(refused)) => {
            eprintln!("hub: {} refused a forge check ({refused})", repo.full_name);
            "refused"
        }
        // Not applied; the row stays `attempting`.
        Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => return Err(error),
        // Uncertain: the status may exist.
        Err(GhError::Failed(detail)) => {
            eprintln!(
                "hub: forge check post for {}#{number}: {detail}",
                repo.full_name
            );
            return Ok(Posting::Unknown);
        }
    };
    // Replacing a row that may show a success: a refusal means the success is still there, so
    // the row stays `attempting` and the next pass tries again.
    if phase == "refused" && !desired.final_on_refusal && row.is_some_and(may_show_success) {
        return Ok(Posting::Refused);
    }
    let (store, id, ctx) = (state.store.clone(), repo.id.clone(), context.to_owned());
    let recorded = blocking(state, move || {
        store.complete_forge_check(&id, number, &ctx, generation, phase, unix_now())
    })
    .await?;
    Ok(match (phase, recorded) {
        ("posted", true) => Posting::Posted,
        ("posted", false) => Posting::Unrecorded,
        (_, true) => Posting::Refused,
        // Refused but unrecorded: the row stays `attempting` and is retried.
        _ => Posting::Unknown,
    })
}

#[cfg(test)]
mod tests;
