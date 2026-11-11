//! Explicit named-file consent, separate from Git conflicts and review.
use serde::{Deserialize, Serialize};

use super::coordination::{
    CoordinationAgent, CoordinationProposal, CoordinationRequest, CoordinationStatus,
};
use super::intent::{overlaps, spec_key, text};
use super::work::{WorkItem, validate_id};
use super::{HubError, Result};

pub const MAX_OVERLAP_PATHS: usize = 256;
pub const MAX_OVERLAP_CONSENTS: usize = 100;

/// One target lead's consent against an exact retained coordination proposal. Requester
/// acceptance is a separate operation; creating this record alone never authorizes an edit.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOverlapConsentRequest {
    pub request_id: String,
    pub target_work_id: String,
    pub operation_id: String,
    pub session: String,
    pub expected_updated_seq: i64,
    pub target_generation: i64,
    pub target_updated_seq: i64,
    pub paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOverlapFence {
    pub work_id: String,
    pub lead: CoordinationAgent,
    pub generation: i64,
    /// Changes on scope, lead, reservation release or re-admission, never ordinary progress.
    pub epoch: i64,
    pub scope: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOverlapConsent {
    pub id: String,
    pub request_id: String,
    pub request_version: i64,
    pub requester: CoordinationAgent,
    pub proposal: CoordinationProposal,
    pub target: WorkOverlapFence,
    pub paths: Vec<String>,
    /// Set atomically with requester acceptance and admission, never by the target lead.
    pub accepted: Option<WorkOverlapFence>,
    pub created_at: i64,
    pub event_seq: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkOverlapConsentState {
    AwaitingRequester,
    Agreed,
    Invalidated,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOverlapConsentView {
    pub consent: WorkOverlapConsent,
    pub state: WorkOverlapConsentState,
}

impl WorkOverlapConsentRequest {
    pub(super) fn validate(&self) -> Result<Vec<String>> {
        validate_id("coordination request", &self.request_id, "crq")?;
        validate_id("target work", &self.target_work_id, "wrk")?;
        text("operation_id", &self.operation_id, 255)?;
        text("session", &self.session, 255)?;
        if self.expected_updated_seq < 1
            || self.target_generation < 1
            || self.target_updated_seq < 1
        {
            return Err(invalid("overlap consent requires positive version fences"));
        }
        named_paths(&self.paths)
    }
}

pub(super) fn named_paths(paths: &[String]) -> Result<Vec<String>> {
    if paths.is_empty() || paths.len() > MAX_OVERLAP_PATHS {
        return Err(invalid(
            "overlap consent requires 1 to 256 exact file paths",
        ));
    }
    let mut result = Vec::with_capacity(paths.len());
    for path in paths {
        let key = spec_key(path)?;
        if key.ends_with('/') {
            return Err(invalid(
                "overlap consent names exact files, not directories",
            ));
        }
        result.push(key);
    }
    result.sort();
    if result.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(invalid("overlap consent contains duplicate paths"));
    }
    Ok(result)
}

pub(super) fn covered(scope: &[String], path: &str) -> bool {
    scope.iter().any(|entry| overlaps(entry, path))
}

pub(super) fn fence(item: &WorkItem, epoch: i64) -> WorkOverlapFence {
    WorkOverlapFence {
        work_id: item.id.clone(),
        lead: CoordinationAgent {
            token: item.lead_token.clone(),
            agent: item.lead_agent.clone(),
            session: item.lead_session.clone(),
            owner: item.owner.clone(),
            login: item.owner_login.clone(),
        },
        generation: item.generation,
        epoch,
        scope: item.scope.clone(),
    }
}

pub(super) fn matches(fence: &WorkOverlapFence, item: &WorkItem, epoch: i64, live: bool) -> bool {
    live && item.claim_status != super::work::ClaimStatus::Unknown
        && fence.work_id == item.id
        && fence.generation == item.generation
        && fence.epoch == epoch
        && fence.lead.token == item.lead_token
        && fence.lead.session == item.lead_session
        && fence.scope == item.scope
}

pub(super) fn validate_target(
    request: &CoordinationRequest,
    input: &WorkOverlapConsentRequest,
    item: &WorkItem,
    token: &str,
    live: bool,
    paths: &[String],
) -> Result<()> {
    if item.lead_token != token
        || item.lead_session != input.session
        || !request.targets.iter().any(|target| {
            target.work_id == item.id
                && target.generation == item.generation
                && target.lead.token == token
                && target.lead.session == input.session
        })
    {
        return Err(HubError::Unauthorized);
    }
    if !matches!(
        request.status,
        CoordinationStatus::Pending | CoordinationStatus::Responded
    ) || request.updated_seq != input.expected_updated_seq
        || item.generation != input.target_generation
        || item.updated_seq != input.target_updated_seq
        || !live
    {
        return Err(invalid(
            "overlap consent subject changed or its reservation lapsed",
        ));
    }
    if paths
        .iter()
        .any(|path| !covered(&request.proposal.scope, path) || !covered(&item.scope, path))
    {
        return Err(invalid(
            "overlap consent path is outside the proposal or target scope",
        ));
    }
    Ok(())
}

pub(super) fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.into())
}

/// Explicit requester consent to the named grants, evaluated with ordinary admission rules.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOverlapStart {
    pub start: super::work::WorkStart,
    pub consent_ids: Vec<String>,
}

/// Scope expansion uses the existing checkpoint fence and receipt.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkOverlapCheckpoint {
    pub checkpoint: super::work::WorkCheckpoint,
    pub consent_ids: Vec<String>,
}

pub(super) fn consent_ids(ids: &[String]) -> Result<()> {
    if ids.is_empty() || ids.len() > MAX_OVERLAP_CONSENTS {
        return Err(invalid("acceptance requires 1 to 100 consent IDs"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        validate_id("overlap consent", id, "woc")?;
        if !seen.insert(id) {
            return Err(invalid("duplicate overlap consent ID"));
        }
    }
    Ok(())
}

/// Inspect all scope intersections; the bounded display witnesses are not permission evidence.
pub(super) fn unresolved(ours: &[String], theirs: &[String], agreed: &[String]) -> bool {
    ours.iter().any(|a| {
        theirs.iter().any(|b| {
            overlaps(a, b)
                && match (a.ends_with('/'), b.ends_with('/')) {
                    (true, true) => true,
                    (false, _) => !agreed.contains(a),
                    (_, false) => !agreed.contains(b),
                }
        })
    })
}

pub(super) fn annotate(
    overlaps: &mut [super::work::WorkScopeOverlap],
    consents: &[WorkOverlapConsent],
) {
    for overlap in overlaps {
        for consent in consents {
            if consent.target.work_id == overlap.item.id {
                overlap.agreed_paths.extend(consent.paths.clone());
            }
        }
        overlap.agreed_paths.sort();
        overlap.agreed_paths.dedup();
    }
}

#[cfg(test)]
mod tests {
    use super::unresolved;
    #[test]
    fn named_file_consent_never_covers_a_directory_or_an_omitted_intersection() {
        assert!(!unresolved(
            &["src/".into()],
            &["src/a.rs".into()],
            &["src/a.rs".into()]
        ));
        assert!(unresolved(
            &["src/".into()],
            &["src/".into()],
            &["src/a.rs".into()]
        ));
        let files: Vec<String> = (0..6).map(|i| format!("src/{i}.rs")).collect();
        assert!(unresolved(&files, &["src/".into()], &files[..5]));
    }
}
