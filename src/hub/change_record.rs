//! Shared change-record projection over authoritative work and retained capture facts.

use crate::change_record::{
    ActionFinishV1, ActionOutcomeV1, ActionStartV1, ActionSummaryV1, ApplicabilityV1,
    AssuranceBasisV1, CapturedAttributionV1, CapturedEventV1, ChangeRecordSummaryV1, CheckResultV1,
    CompletenessV1, ContentAvailabilityV1, ConversationEntryV1, CoverageV1, DecisionAuthorKindV1,
    DecisionSourceRefV1, DecisionStatusV1, DecisionSummaryV1, DiffReferenceV1, DriftV1,
    IntentSummaryV1, MAX_PROJECTED_DECISION_REFS, MAX_PROJECTED_SCOPE, RecipeProvenanceV1,
    ReviewProjectionV1, SessionCoverageV1, SessionSetupV1, SourceBindingV1, SubjectSummaryV1,
    UnknownFactV1,
};

use super::viewer::Viewer;
use super::work::{Checkpoint, SourceRef, WorkItem};
use super::{HubError, Result};

/// Lifecycle of one shared-conversation body declaration, keyed by (work, digest, author).
/// `Declared` means an accepted event named the body but its author uploaded no bytes. The object
/// is shared by every author's row for the same digest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SharedBodyState {
    Declared,
    Available,
    Expired,
    Deleted,
}

impl SharedBodyState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Available => "available",
            Self::Expired => "expired",
            Self::Deleted => "deleted",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "declared" => Ok(Self::Declared),
            "available" => Ok(Self::Available),
            "expired" => Ok(Self::Expired),
            "deleted" => Ok(Self::Deleted),
            other => Err(HubError::Unavailable(format!(
                "unknown shared body state {other}"
            ))),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedBodyRow {
    pub work: String,
    pub digest: String,
    /// The human user whose token declared the body; only they and Hub admins may read or delete it.
    pub author: String,
    pub size_bytes: u64,
    pub session_id: String,
    pub generation: i64,
    pub event_id: String,
    pub sequence: u64,
    pub declared_token: String,
    pub declared_at: i64,
    pub state: SharedBodyState,
    pub uploaded_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub deleted_at: Option<i64>,
    pub deleted_by: Option<String>,
    pub physical_deleted_at: Option<i64>,
    /// Failed object removals so far; drives the sweep's retry backoff.
    pub delete_attempts: i64,
}

impl SharedBodyRow {
    /// The state a reader must act on at `now`: a body is expired from its `expires_at` even
    /// before the sweep records it.
    pub fn state_at(&self, now: i64) -> SharedBodyState {
        match (self.state, self.expires_at) {
            (SharedBodyState::Available, Some(expires)) if now >= expires => {
                SharedBodyState::Expired
            }
            (state, _) => state,
        }
    }

    /// What a listing says about the body at `now`. A missing or corrupt object is not visible
    /// here; reading the body reports it.
    fn availability_at(&self, now: i64) -> ContentAvailabilityV1 {
        match self.state_at(now) {
            SharedBodyState::Declared => ContentAvailabilityV1::Local,
            SharedBodyState::Available => ContentAvailabilityV1::Shared,
            SharedBodyState::Expired => ContentAvailabilityV1::Expired,
            SharedBodyState::Deleted => ContentAvailabilityV1::Deleted,
        }
    }

    /// The listing entry for this body and the retained JSON of the event that declared it.
    pub(super) fn conversation_entry(
        &self,
        event: &str,
        observed_at: i64,
        now: i64,
    ) -> Result<ConversationEntryV1> {
        let event: CapturedEventV1 = serde_json::from_str(event)
            .map_err(|error| HubError::Unavailable(error.to_string()))?;
        Ok(ConversationEntryV1 {
            session_id: self.session_id.clone(),
            sequence: self.sequence,
            event_id: self.event_id.clone(),
            kind: event.kind,
            observed_at,
            digest: self.digest.clone(),
            size_bytes: self.size_bytes,
            availability: self.availability_at(now),
            expires_at: self.expires_at,
            // The user id; the read swaps in the login (`conversation_page`).
            author: Some(self.author.clone()),
        })
    }
}

/// The one access rule for shared conversation bodies and their listing, for reading and
/// deleting alike: the human who declared the body (through any of their tokens) and Hub admins.
/// The Work owner has no access of their own. Every HTTP and MCP path goes through this; the
/// listing applies it in the store query.
pub fn conversation_readable(author: &str, caller: &str, admin: bool) -> bool {
    admin || author == caller
}

/// Whose shared bodies a listing covers besides the caller's own, which it always covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConversationAudience {
    /// Nobody else's.
    Own,
    /// Every author who has not opted out of sharing in the repository: what a reader sees while
    /// the repository shares. Opting out hides what the author shared before as well.
    Sharing,
    /// Every author's: a Hub admin.
    Everyone,
}

/// Every declaration of one digest in a Work, with who is asking.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedBodyAccess {
    pub rows: Vec<SharedBodyRow>,
    /// The caller's human owner.
    pub caller: String,
}

impl SharedBodyAccess {
    /// The caller's own declaration, whatever its state.
    pub fn own(&self) -> Option<&SharedBodyRow> {
        self.rows.iter().find(|row| row.author == self.caller)
    }

    /// The declaration a read or delete acts on: an available one (the caller's own first), else
    /// the caller's own, else for an admin the first of the others. `None` means the caller may not act, whether or not the digest
    /// exists.
    pub fn accessible(&self, admin: bool, now: i64) -> Option<&SharedBodyRow> {
        self.rows
            .iter()
            .filter(|row| conversation_readable(&row.author, &self.caller, admin))
            .min_by_key(|row| {
                (
                    row.state_at(now) != SharedBodyState::Available,
                    row.author != self.caller,
                )
            })
    }
}

#[derive(Clone, Debug)]
pub struct StoredActionV1 {
    pub start: ActionStartV1,
    pub finish: Option<ActionFinishV1>,
    pub source_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct SessionStatsV1 {
    pub count: usize,
    pub gap_count: usize,
    pub complete_count: usize,
    pub setup_observation_count: usize,
    pub usage_observation_count: usize,
    /// Latest setup per session (at most `MAX_PROJECTED_SESSION_SETUPS`) and the count beyond them.
    pub setups: Vec<SessionSetupV1>,
    pub setups_omitted: u64,
}

#[derive(Clone, Debug)]
pub struct StoredDecisionV1 {
    pub event_seq: i64,
    pub checkpoint: Checkpoint,
}

/// The record as `viewer` may see it: another owner's captured sessions by their digests.
pub fn project(
    item: &WorkItem,
    sessions: SessionStatsV1,
    action: Option<StoredActionV1>,
    decision: Option<StoredDecisionV1>,
    as_of: i64,
    viewer: Viewer<'_>,
) -> ChangeRecordSummaryV1 {
    let session_coverage = if sessions.count == 0 {
        CoverageV1::Unknown
    } else if sessions.gap_count == 0 && sessions.complete_count == sessions.count {
        CoverageV1::Complete
    } else {
        CoverageV1::Partial
    };
    let subject = action.as_ref().map(|stored| SubjectSummaryV1 {
        source_id: stored.source_id.clone(),
        source_kind: stored.start.source.kind(),
        source: stored.start.source.clone(),
    });
    let latest_action = action.as_ref().map(|stored| action_summary(item, stored));
    let captured_by = action.as_ref().map(|stored| {
        let mut session_id = stored.start.session_id.clone();
        let (owner, token) = Viewer::capturer(
            &item.participants,
            &session_id,
            (Some(item.owner.as_str()), item.lead_token.as_str()),
        );
        viewer.session(owner, token, &mut session_id);
        let mut agent = item
            .participants
            .iter()
            .find(|participant| participant.session == stored.start.session_id)
            .map_or_else(
                || item.lead_agent.clone(),
                |participant| participant.agent.clone(),
            );
        viewer.label(owner, &mut agent);
        CapturedAttributionV1 { agent, session_id }
    });
    let diff = action.as_ref().map(|stored| match &stored.start.source {
        SourceBindingV1::Native {
            repository,
            revision,
            change_id,
            ..
        } => DiffReferenceV1::Native {
            repository: repository.clone(),
            revision: revision.clone(),
            change_id: change_id.clone(),
        },
        SourceBindingV1::Git {
            repository,
            worktree_id,
            base,
            head,
            dirty,
            untracked_count,
            omissions,
            snapshot_coverage,
            ..
        } => DiffReferenceV1::Git {
            repository: repository.clone(),
            worktree_id: worktree_id.clone(),
            base: base.clone(),
            head: head.clone(),
            dirty: *dirty,
            untracked_count: *untracked_count,
            omissions: omissions.clone(),
            snapshot_coverage: *snapshot_coverage,
        },
    });
    let result_coverage = latest_action
        .as_ref()
        .map_or(CoverageV1::Unknown, |action| {
            if action.result == CheckResultV1::Unknown {
                CoverageV1::Unknown
            } else {
                action.coverage
            }
        });
    let setup_coverage = if sessions.setup_observation_count == 0 {
        CoverageV1::Unknown
    } else {
        CoverageV1::Partial
    };
    let usage_coverage = if sessions.usage_observation_count == 0 {
        CoverageV1::Unknown
    } else {
        CoverageV1::Partial
    };
    let mut unknowns = Vec::new();
    if session_coverage == CoverageV1::Unknown {
        unknowns.push(UnknownFactV1::CaptureCoverage);
    }
    if latest_action.is_none() {
        unknowns.extend([
            UnknownFactV1::SourceApplicability,
            UnknownFactV1::RecipeIdentity,
            UnknownFactV1::Result,
        ]);
    } else if let Some(action) = &latest_action {
        if action.recipe_provenance != RecipeProvenanceV1::ConfiguredExactMatch {
            unknowns.push(UnknownFactV1::RecipeIdentity);
        }
        if action.applicability == ApplicabilityV1::Unknown {
            unknowns.push(UnknownFactV1::SourceApplicability);
        }
        if action.result == CheckResultV1::Unknown {
            unknowns.push(UnknownFactV1::Result);
        }
    }
    if setup_coverage == CoverageV1::Unknown {
        unknowns.push(UnknownFactV1::SetupIdentity);
    }
    if usage_coverage == CoverageV1::Unknown {
        unknowns.push(UnknownFactV1::Usage);
    }
    unknowns.push(UnknownFactV1::HumanDecision);
    let decision = decision_summary(item, decision, viewer);
    let mut session_setups = sessions.setups;
    for setup in &mut session_setups {
        // Capture comes from a participant's session; a parent session is that same agent's.
        let (owner, token) = Viewer::capturer(
            &item.participants,
            &setup.session_id,
            (None, &item.lead_token),
        );
        let sessions = std::iter::once(&mut setup.session_id).chain(&mut setup.parent_session_id);
        for session in sessions {
            viewer.session(owner, token, session);
        }
    }
    let review_concerns =
        review_concerns(session_coverage, latest_action.as_ref(), action.as_ref());
    let mut next_actor = item.lead_agent.clone();
    viewer.label(Some(&item.owner), &mut next_actor);
    ChangeRecordSummaryV1 {
        schema_version: crate::change_record::SCHEMA_VERSION_V1,
        work_id: item.id.clone(),
        generation: item.generation,
        as_of,
        intent: IntentSummaryV1 {
            title: item.title.clone(),
            description: item.description.clone(),
            repository: item.repository.clone(),
            branch: item.branch.clone(),
            provenance: match item.source {
                super::work::WorkSource::Native { .. } => "native",
                super::work::WorkSource::Forge { .. } => "forge",
            }
            .into(),
            scope: item
                .scope
                .iter()
                .take(MAX_PROJECTED_SCOPE)
                .cloned()
                .collect(),
            scope_omitted: item.scope.len().saturating_sub(MAX_PROJECTED_SCOPE),
        },
        subject,
        sessions: SessionCoverageV1 {
            count: sessions.count,
            coverage: session_coverage,
            gaps: sessions.gap_count,
        },
        session_setups,
        session_setups_omitted: sessions.setups_omitted,
        latest_action,
        decision,
        next_action: item.next_action.clone(),
        completeness: CompletenessV1 {
            events: session_coverage,
            source: action
                .as_ref()
                .map_or(CoverageV1::Unknown, |stored| stored.start.source.coverage()),
            // Digests supplied by the capture caller identify bytes but do not prove setup provenance.
            setup: setup_coverage,
            recipe: action.as_ref().map_or(CoverageV1::Unknown, |stored| {
                if stored.start.recipe_provenance == RecipeProvenanceV1::ConfiguredExactMatch {
                    CoverageV1::Partial
                } else {
                    CoverageV1::Unknown
                }
            }),
            result: result_coverage,
            usage: usage_coverage,
        },
        unknowns,
        review: Some(ReviewProjectionV1 {
            captured_by,
            diff,
            actual_scope: CoverageV1::Unknown,
            concerns: review_concerns,
            detail_links: vec![
                format!("work:{}:change-record", item.id),
                format!("work:{}:conversation", item.id),
                format!("work:{}:actionable-status", item.id),
            ],
            next_actor: Some(next_actor),
            supported_action: None,
            establishes_approval: false,
        }),
    }
}

fn review_concerns(
    sessions: CoverageV1,
    latest: Option<&ActionSummaryV1>,
    stored: Option<&StoredActionV1>,
) -> Vec<String> {
    let mut concerns = Vec::new();
    if sessions != CoverageV1::Complete {
        concerns.push("capture_history_incomplete_or_unknown".into());
    }
    if latest.is_none() {
        concerns.push("no_captured_action".into());
    }
    if latest.is_some_and(|action| action.outcome != ActionOutcomeV1::Succeeded) {
        concerns.push("latest_execution_not_successful".into());
    }
    if latest.is_some_and(|action| action.result != CheckResultV1::Passed) {
        concerns.push("latest_check_not_passing".into());
    }
    if latest.is_some_and(|action| action.applicability != ApplicabilityV1::Current) {
        concerns.push("current_source_applicability_not_established".into());
    }
    if let Some(stored) = stored
        && let SourceBindingV1::Git {
            dirty,
            untracked_count,
            omissions,
            ..
        } = &stored.start.source
    {
        if *dirty || *untracked_count > 0 {
            concerns.push("captured_source_dirty_or_untracked".into());
        }
        if !omissions.is_empty() {
            concerns.push("captured_source_has_omissions".into());
        }
    }
    concerns.push("actual_changed_scope_requires_source_diff".into());
    concerns.push("supported_next_action_requires_actionable_status".into());
    concerns.push("approval_and_readiness_not_established".into());
    concerns
}

fn decision_summary(
    item: &WorkItem,
    stored: Option<StoredDecisionV1>,
    viewer: Viewer<'_>,
) -> DecisionSummaryV1 {
    let Some(mut stored) = stored else {
        return DecisionSummaryV1 {
            status: DecisionStatusV1::Missing,
            reference: None,
            summary: None,
            source_refs: Vec::new(),
            source_refs_omitted: 0,
            author_kind: None,
            author_agent: None,
            at: None,
            generation: None,
        };
    };
    let source_refs_omitted = stored
        .checkpoint
        .source_refs
        .len()
        .saturating_sub(MAX_PROJECTED_DECISION_REFS);
    let author = item
        .participants
        .iter()
        .find(|p| p.token == stored.checkpoint.token && p.session == stored.checkpoint.session)
        .map(|p| p.owner.as_str());
    viewer.label(author, &mut stored.checkpoint.agent);
    let reporter = (author, stored.checkpoint.token.as_str());
    DecisionSummaryV1 {
        // A Work Decision is an authorized agent checkpoint. It does not prove human approval.
        status: DecisionStatusV1::Proposed,
        reference: Some(format!("work:{}:event:{}", item.id, stored.event_seq)),
        summary: Some(stored.checkpoint.summary),
        source_refs: stored
            .checkpoint
            .source_refs
            .into_iter()
            .take(MAX_PROJECTED_DECISION_REFS)
            .map(|reference| match reference {
                SourceRef::RepositoryPath { path, line } => {
                    DecisionSourceRefV1::RepositoryPath { path, line }
                }
                SourceRef::Commit { oid, path } => DecisionSourceRefV1::Commit { oid, path },
                SourceRef::Work { work_id } => DecisionSourceRefV1::Work { work_id },
                SourceRef::CapturedEvent {
                    mut session_id,
                    event_id,
                } => {
                    let (owner, token) =
                        Viewer::capturer(&item.participants, &session_id, reporter);
                    viewer.session(owner, token, &mut session_id);
                    DecisionSourceRefV1::CapturedEvent {
                        session_id,
                        event_id,
                    }
                }
                SourceRef::PriorWorkVersion { work_id, event_seq } => {
                    DecisionSourceRefV1::PriorWorkVersion { work_id, event_seq }
                }
            })
            .collect(),
        source_refs_omitted,
        author_kind: Some(DecisionAuthorKindV1::Agent),
        author_agent: Some(stored.checkpoint.agent),
        at: Some(stored.checkpoint.at),
        generation: Some(stored.checkpoint.generation),
    }
}

fn action_summary(item: &WorkItem, stored: &StoredActionV1) -> ActionSummaryV1 {
    let (outcome, result, assurance, coverage, finished_at, drift, availability) =
        stored.finish.as_ref().map_or_else(
            || {
                (
                    ActionOutcomeV1::Unknown,
                    CheckResultV1::Unknown,
                    AssuranceBasisV1::CallerAsserted,
                    CoverageV1::Unknown,
                    None,
                    DriftV1::Unknown,
                    ContentAvailabilityV1::Missing,
                )
            },
            |finish| {
                (
                    finish.outcome,
                    finish.result,
                    finish.assurance,
                    finish.coverage,
                    Some(finish.finished_at),
                    finish.drift,
                    aggregate_availability(&finish.output_artifacts),
                )
            },
        );
    let applicability = if stored.start.generation != item.generation
        || !matches!(drift, DriftV1::NoneObserved | DriftV1::Unknown)
    {
        ApplicabilityV1::Stale
    } else {
        // The stored work generation alone cannot prove the Git/native source is still current.
        ApplicabilityV1::Unknown
    };
    ActionSummaryV1 {
        action_id: stored.start.action_id.clone(),
        source_id: stored.source_id.clone(),
        kind: stored.start.kind,
        outcome,
        recipe_provenance: stored.start.recipe_provenance,
        recipe_digest: stored.start.recipe_digest.clone(),
        setup_digest: stored.start.setup_digest.clone(),
        result,
        applicability,
        assurance,
        coverage,
        evidence_availability: availability,
        started_at: stored.start.started_at,
        finished_at,
        drift,
    }
}

fn aggregate_availability(
    artifacts: &[crate::change_record::ArtifactRefV1],
) -> ContentAvailabilityV1 {
    use ContentAvailabilityV1 as Availability;
    for state in [
        Availability::IntegrityFailure,
        Availability::Denied,
        Availability::Deleted,
        Availability::Expired,
        Availability::Missing,
    ] {
        if artifacts
            .iter()
            .any(|artifact| artifact.availability == state)
        {
            return state;
        }
    }
    if artifacts.is_empty() {
        Availability::Missing
    } else if artifacts
        .iter()
        .all(|artifact| artifact.availability == Availability::Shared)
    {
        Availability::Shared
    } else if artifacts
        .iter()
        .all(|artifact| artifact.availability == Availability::Local)
    {
        Availability::Local
    } else if artifacts
        .iter()
        .all(|artifact| artifact.availability == Availability::External)
    {
        Availability::External
    } else {
        Availability::Mixed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(author: &str, state: SharedBodyState) -> SharedBodyRow {
        SharedBodyRow {
            work: "w".into(),
            digest: "0".repeat(64),
            author: author.into(),
            size_bytes: 1,
            session_id: "s".into(),
            generation: 1,
            event_id: "e".into(),
            sequence: 1,
            declared_token: "t".into(),
            declared_at: 1,
            state,
            uploaded_at: Some(1),
            expires_at: Some(10),
            deleted_at: None,
            deleted_by: None,
            physical_deleted_at: None,
            delete_attempts: 0,
        }
    }

    #[test]
    fn only_the_author_and_admins_may_act_and_an_admin_prefers_a_live_copy() {
        assert!(conversation_readable("a", "a", false));
        assert!(!conversation_readable("a", "b", false));
        assert!(conversation_readable("a", "b", true));
        let access = SharedBodyAccess {
            rows: vec![
                row("a", SharedBodyState::Deleted),
                row("b", SharedBodyState::Available),
            ],
            caller: "c".into(),
        };
        assert!(access.own().is_none() && access.accessible(false, 0).is_none());
        assert_eq!(access.accessible(true, 0).unwrap().author, "b");
        // Past its deadline the available copy is no better than the deleted one.
        assert_eq!(access.accessible(true, 10).unwrap().author, "a");
        let access = SharedBodyAccess {
            caller: "a".into(),
            ..access
        };
        assert_eq!(access.accessible(false, 0).unwrap().author, "a");
        // An admin prefers a live copy even over their own deleted one; others only see their own.
        assert_eq!(access.accessible(true, 0).unwrap().author, "b");
        assert!(
            SharedBodyAccess {
                rows: vec![],
                caller: "a".into()
            }
            .accessible(true, 0)
            .is_none()
        );
    }

    #[test]
    fn available_bodies_expire_at_their_deadline_before_any_sweep() {
        let mut row = row("a", SharedBodyState::Available);
        assert_eq!(row.state_at(9), SharedBodyState::Available);
        assert_eq!(row.state_at(10), SharedBodyState::Expired);
        row.state = SharedBodyState::Deleted;
        assert_eq!(row.state_at(99), SharedBodyState::Deleted);
    }

    #[test]
    fn availability_is_not_inferred_from_result() {
        assert_eq!(aggregate_availability(&[]), ContentAvailabilityV1::Missing);
    }
}
