//! Pure decision for a Hub-published forge commit status on a candidate pull request.
//!
//! Success needs the Hub's own read of an operator-pinned native store and of the live pull
//! request head. Webhook or comment text is never an input here. Nothing in this module
//! performs network I/O or writes state.
use sha2::{Digest, Sha256};

use super::workspace_execution::AgentTaskProfile;
use super::{HubError, Result};
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

#[cfg(test)]
mod tests;
