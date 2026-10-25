//! Shared change-record projection over authoritative work and retained capture facts.

use crate::change_record::{
    ActionFinishV1, ActionOutcomeV1, ActionStartV1, ActionSummaryV1, ApplicabilityV1,
    AssuranceBasisV1, CapturedEventV1, ChangeRecordSummaryV1, CheckResultV1, CompletenessV1,
    ContentAvailabilityV1, ConversationEntryV1, CoverageV1, DecisionAuthorKindV1,
    DecisionSourceRefV1, DecisionStatusV1, DecisionSummaryV1, DriftV1, IntentSummaryV1,
    MAX_PROJECTED_DECISION_REFS, MAX_PROJECTED_SCOPE, RecipeProvenanceV1, SessionCoverageV1,
    SessionSetupV1, SubjectSummaryV1, UnknownFactV1,
};

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

    /// The declaration a read or delete acts on: the caller's own, else for an admin the first
    /// that is still available. `None` means the caller may not act, whether or not the digest
    /// exists.
    pub fn accessible(&self, admin: bool, now: i64) -> Option<&SharedBodyRow> {
        self.rows
            .iter()
            .filter(|row| conversation_readable(&row.author, &self.caller, admin))
            .min_by_key(|row| {
                (
                    row.author != self.caller,
                    row.state_at(now) != SharedBodyState::Available,
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

pub fn project(
    item: &WorkItem,
    sessions: SessionStatsV1,
    action: Option<StoredActionV1>,
    decision: Option<StoredDecisionV1>,
    as_of: i64,
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
    let decision = decision_summary(item, decision);
    ChangeRecordSummaryV1 {
        schema_version: crate::change_record::SCHEMA_VERSION_V1,
        work_id: item.id.clone(),
        generation: item.generation,
        as_of,
        intent: IntentSummaryV1 {
            title: item.title.clone(),
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
        session_setups: sessions.setups,
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
    }
}

fn decision_summary(item: &WorkItem, stored: Option<StoredDecisionV1>) -> DecisionSummaryV1 {
    let Some(stored) = stored else {
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
                    session_id,
                    event_id,
                } => DecisionSourceRefV1::CapturedEvent {
                    session_id,
                    event_id,
                },
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
        assert_eq!(access.accessible(true, 0).unwrap().author, "a");
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
