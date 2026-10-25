//! Read-only projection of source-bound action receipts and retained visible conversation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::change_capture::spool::{ReadOnlyEnvelopeState, ReadOnlySpool, ReadOnlySpoolProjection};
use crate::change_capture::{CaptureError, sha256};
use crate::change_record::{
    ActionFinishV1, ActionKindV1, ActionOutcomeV1, ActionStartV1, AgentHarnessV1, ArtifactMediaV1,
    AssuranceBasisV1, CaptureEnvelopeV1, CaptureModeV1, CapturedEventKindV1, CheckResultV1,
    ContentAvailabilityV1, CoverageV1, DriftV1, GapFactV1, RecipeProvenanceV1, SourceBindingV1,
    ToolFactV1,
};
use crate::{Error, Result};

const MAX_PAGE_LIMIT: usize = 100;
const MAX_CURSOR_BYTES: usize = 192;
const MAX_BODY_ARTIFACT_BYTES: u64 = 256 * 1024;
const MAX_CONVERSATION_PAGE_BYTES: usize = 3 * 1024 * 1024;

pub struct EvidenceExplorer {
    spool: ReadOnlySpool,
    repository: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRelationV1 {
    ExactSourceBinding,
    SameWorkSession,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PathRelationV1 {
    NotRequested,
    NotRecorded,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceApplicabilityV1 {
    MatchingRecordedCleanSource,
    DifferentRecordedSource,
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RetainedEnvelopeStateV1 {
    Pending,
    Acknowledged,
    Compacted,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum BodyAvailabilityV1 {
    Available,
    NotCaptured,
    NotApplicable,
    Missing,
    Expired,
    Deleted,
    Denied,
    TooLarge,
    IntegrityFailure,
    InvalidBody,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionResultEvidenceV1 {
    pub operation_id: String,
    pub outcome: ActionOutcomeV1,
    pub result: CheckResultV1,
    pub assurance: AssuranceBasisV1,
    pub coverage: CoverageV1,
    pub drift: DriftV1,
    pub finished_at: i64,
    pub retention: RetainedEnvelopeStateV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActionEvidenceV1 {
    pub relation: EvidenceRelationV1,
    pub path_relation: PathRelationV1,
    pub work_id: String,
    pub generation: i64,
    pub session_id: String,
    pub action_id: String,
    pub operation_id: String,
    pub action_kind: ActionKindV1,
    pub recipe_provenance: RecipeProvenanceV1,
    pub recipe_digest: String,
    pub setup_digest: String,
    pub started_at: i64,
    pub source: SourceBindingV1,
    pub applicability: EvidenceApplicabilityV1,
    pub start_retention: RetainedEnvelopeStateV1,
    pub result: Option<ActionResultEvidenceV1>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionEvidenceV1 {
    pub relation: EvidenceRelationV1,
    pub work_id: String,
    pub generation: i64,
    pub session_id: String,
    pub harness: AgentHarnessV1,
    pub capture_mode: CaptureModeV1,
    pub first_observed_at: i64,
    pub last_observed_at: i64,
    pub event_count: usize,
    pub unavailable_body_count: usize,
    pub capture_gap_count: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "entry", rename_all = "snake_case")]
pub enum EvidenceEntryV1 {
    Action(Box<ActionEvidenceV1>),
    Session(SessionEvidenceV1),
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProjectionCoverageV1 {
    pub retained_envelopes: usize,
    pub compacted_event_batches_unattributed: usize,
    pub unavailable_bodies: usize,
    pub unverified_bodies: usize,
    pub capture_gaps: usize,
    pub complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct EvidencePageV1 {
    pub version: u8,
    pub repository: String,
    pub commit: String,
    pub path: Option<String>,
    pub requested_work_id: Option<String>,
    pub entries: Vec<EvidenceEntryV1>,
    pub projection_coverage: ProjectionCoverageV1,
    pub next_cursor: Option<String>,
    pub complete: bool,
}

/// Retained sessions for this repository, newest first. Each belongs to its Work; none of them is
/// claimed to be associated with any source revision.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionPageV1 {
    pub version: u8,
    pub repository: String,
    pub sessions: Vec<SessionEvidenceV1>,
    pub projection_coverage: ProjectionCoverageV1,
    pub next_cursor: Option<String>,
    pub complete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationBodyV1 {
    pub availability: BodyAvailabilityV1,
    pub media: Option<ArtifactMediaV1>,
    pub content: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationEventV1 {
    pub work_id: String,
    pub generation: i64,
    pub session_id: String,
    pub event_id: String,
    pub sequence: u64,
    pub observed_at: i64,
    pub kind: CapturedEventKindV1,
    pub tool: Option<ToolFactV1>,
    pub relation: EvidenceRelationV1,
    pub body: ConversationBodyV1,
    pub gap: Option<GapFactV1>,
    pub retention: RetainedEnvelopeStateV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ConversationPageV1 {
    pub version: u8,
    pub repository: String,
    pub work_id: String,
    pub session_id: String,
    pub relation: EvidenceRelationV1,
    pub events: Vec<ConversationEventV1>,
    pub projection_coverage: ProjectionCoverageV1,
    pub next_cursor: Option<String>,
    pub complete: bool,
}

#[derive(Clone)]
struct RetainedAction {
    start: ActionStartV1,
    start_state: ReadOnlyEnvelopeState,
    finish: Option<(ActionFinishV1, ReadOnlyEnvelopeState)>,
}

#[derive(Clone)]
struct RetainedEvent {
    work_id: String,
    generation: i64,
    session_id: String,
    capture_mode: CaptureModeV1,
    event: crate::change_record::CapturedEventV1,
    state: ReadOnlyEnvelopeState,
}

#[derive(Clone)]
struct SessionAggregate {
    work_id: String,
    generation: i64,
    session_id: String,
    harness: AgentHarnessV1,
    capture_mode: CaptureModeV1,
    first_observed_at: i64,
    last_observed_at: i64,
    event_count: usize,
    unavailable_body_count: usize,
    unverified_body_count: usize,
    capture_gap_count: usize,
}

impl EvidenceExplorer {
    pub fn open(spool: impl AsRef<Path>, repository_id: impl Into<String>) -> Result<Self> {
        let repository = repository_id.into();
        if !bounded_repository(&repository) {
            return Err(Error::Invalid(
                "explorer repository identity is invalid".into(),
            ));
        }
        let spool = ReadOnlySpool::open(spool, &repository).map_err(map_capture_error)?;
        Ok(Self { spool, repository })
    }

    pub fn evidence(
        &self,
        commit: &str,
        path: Option<&str>,
        work_id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<EvidencePageV1> {
        validate_commit(commit)?;
        validate_limit(limit)?;
        let path = path.map(validate_repository_path).transpose()?;
        if let Some(work_id) = work_id {
            validate_identifier("work id", work_id)?;
        }

        let projection = self.spool.projection().map_err(map_capture_error)?;
        let fingerprint = projection_fingerprint(&projection)?;
        let query = query_digest(&[
            "evidence",
            &self.repository,
            commit,
            path.as_deref().unwrap_or(""),
            work_id.unwrap_or(""),
            &fingerprint,
        ]);
        let offset = decode_cursor(cursor, "e", &query)?;
        let (actions, finish_only_actions) = retained_actions(&projection)?;
        let sessions = retained_sessions(&projection)?;

        let mut related_work = BTreeSet::new();
        let mut entries = Vec::new();
        let mut incomplete_selected_actions = 0usize;
        for action in actions {
            if action.start.source.repository() != self.repository {
                continue;
            }
            let requested_work = work_id.is_some_and(|selected| selected == action.start.work_id);
            let exact_head = matches!(
                &action.start.source,
                SourceBindingV1::Git { head, .. } if head == commit
            );
            if work_id.is_some() && !requested_work {
                continue;
            }
            if work_id.is_none() && !exact_head {
                continue;
            }
            incomplete_selected_actions =
                incomplete_selected_actions.saturating_add(usize::from(action.finish.is_none()));
            related_work.insert(action.start.work_id.clone());
            entries.push(EvidenceEntryV1::Action(Box::new(action_evidence(
                action,
                commit,
                path.is_some(),
            ))));
        }
        if let Some(work_id) = work_id {
            related_work.insert(work_id.to_owned());
        }
        let mut unverified_bodies = 0usize;
        for session in sessions {
            if related_work.contains(&session.work_id) {
                unverified_bodies = unverified_bodies.saturating_add(session.unverified_body_count);
                entries.push(EvidenceEntryV1::Session(SessionEvidenceV1 {
                    relation: EvidenceRelationV1::SameWorkSession,
                    work_id: session.work_id,
                    generation: session.generation,
                    session_id: session.session_id,
                    harness: session.harness,
                    capture_mode: session.capture_mode,
                    first_observed_at: session.first_observed_at,
                    last_observed_at: session.last_observed_at,
                    event_count: session.event_count,
                    unavailable_body_count: session.unavailable_body_count,
                    capture_gap_count: session.capture_gap_count,
                }));
            }
        }
        entries.sort_by(|left, right| evidence_entry_key(left).cmp(&evidence_entry_key(right)));

        let unavailable_bodies = entries
            .iter()
            .filter_map(|entry| match entry {
                EvidenceEntryV1::Session(session) => Some(session.unavailable_body_count),
                EvidenceEntryV1::Action(_) => None,
            })
            .sum();
        let session_gaps: usize = entries
            .iter()
            .filter_map(|entry| match entry {
                EvidenceEntryV1::Session(session) => Some(session.capture_gap_count),
                EvidenceEntryV1::Action(_) => None,
            })
            .sum();
        let finish_only_selected_work = work_id.map_or(0, |selected_work| {
            finish_only_actions
                .iter()
                .filter(|(work, _, _)| work == selected_work)
                .count()
        });
        let capture_gaps = session_gaps
            .saturating_add(incomplete_selected_actions)
            .saturating_add(finish_only_selected_work);
        let projection_coverage = coverage(
            &projection,
            unavailable_bodies,
            unverified_bodies,
            capture_gaps,
        );
        let (entries, next_cursor) = page(entries, offset, limit, "e", &query)?;
        let complete = next_cursor.is_none();
        Ok(EvidencePageV1 {
            version: 1,
            repository: self.repository.clone(),
            commit: commit.to_owned(),
            path,
            requested_work_id: work_id.map(str::to_owned),
            entries,
            projection_coverage,
            next_cursor,
            complete,
        })
    }

    /// Every retained session, so a saved conversation is reachable from its Work even when no
    /// recorded action ties it to the commit being viewed.
    pub fn sessions(&self, cursor: Option<&str>, limit: usize) -> Result<SessionPageV1> {
        validate_limit(limit)?;
        let projection = self.spool.projection().map_err(map_capture_error)?;
        let fingerprint = projection_fingerprint(&projection)?;
        let query = query_digest(&["sessions", &self.repository, &fingerprint]);
        let offset = decode_cursor(cursor, "s", &query)?;
        // `conversation` reads a session across its generations, so list it once that way.
        let mut merged = BTreeMap::<(String, String), SessionAggregate>::new();
        for session in retained_sessions(&projection)? {
            match merged.entry((session.work_id.clone(), session.session_id.clone())) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(session);
                }
                std::collections::btree_map::Entry::Occupied(mut entry) => {
                    let kept = entry.get_mut();
                    kept.generation = kept.generation.max(session.generation);
                    kept.first_observed_at = kept.first_observed_at.min(session.first_observed_at);
                    kept.last_observed_at = kept.last_observed_at.max(session.last_observed_at);
                    kept.event_count = kept.event_count.saturating_add(session.event_count);
                    kept.unavailable_body_count = kept
                        .unavailable_body_count
                        .saturating_add(session.unavailable_body_count);
                    kept.unverified_body_count = kept
                        .unverified_body_count
                        .saturating_add(session.unverified_body_count);
                    kept.capture_gap_count = kept
                        .capture_gap_count
                        .saturating_add(session.capture_gap_count);
                }
            }
        }
        let mut sessions = merged.into_values().collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            (right.last_observed_at, &right.work_id, &right.session_id).cmp(&(
                left.last_observed_at,
                &left.work_id,
                &left.session_id,
            ))
        });
        let unavailable_bodies = sessions.iter().map(|s| s.unavailable_body_count).sum();
        let unverified_bodies = sessions.iter().map(|s| s.unverified_body_count).sum();
        let capture_gaps = sessions.iter().map(|s| s.capture_gap_count).sum();
        let projection_coverage = coverage(
            &projection,
            unavailable_bodies,
            unverified_bodies,
            capture_gaps,
        );
        let sessions = sessions
            .into_iter()
            .map(|session| SessionEvidenceV1 {
                relation: EvidenceRelationV1::SameWorkSession,
                work_id: session.work_id,
                generation: session.generation,
                session_id: session.session_id,
                harness: session.harness,
                capture_mode: session.capture_mode,
                first_observed_at: session.first_observed_at,
                last_observed_at: session.last_observed_at,
                event_count: session.event_count,
                unavailable_body_count: session.unavailable_body_count,
                capture_gap_count: session.capture_gap_count,
            })
            .collect();
        let (sessions, next_cursor) = page(sessions, offset, limit, "s", &query)?;
        Ok(SessionPageV1 {
            version: 1,
            repository: self.repository.clone(),
            complete: next_cursor.is_none(),
            sessions,
            projection_coverage,
            next_cursor,
        })
    }

    pub fn conversation(
        &self,
        work_id: &str,
        session_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<ConversationPageV1> {
        validate_identifier("work id", work_id)?;
        validate_identifier("session id", session_id)?;
        validate_limit(limit)?;

        let projection = self.spool.projection().map_err(map_capture_error)?;
        let fingerprint = projection_fingerprint(&projection)?;
        let query = query_digest(&[
            "conversation",
            &self.repository,
            work_id,
            session_id,
            &fingerprint,
        ]);
        let offset = decode_cursor(cursor, "c", &query)?;
        let mut retained = retained_events(&projection)?
            .into_iter()
            .filter(|event| event.work_id == work_id && event.session_id == session_id)
            .collect::<Vec<_>>();
        retained.sort_by(|left, right| {
            (
                left.generation,
                left.event.sequence,
                left.event.observed_at,
                left.event.event_id.as_str(),
            )
                .cmp(&(
                    right.generation,
                    right.event.sequence,
                    right.event.observed_at,
                    right.event.event_id.as_str(),
                ))
        });
        if offset > retained.len() {
            return Err(Error::Invalid("conversation cursor is out of range".into()));
        }

        let mut unavailable_bodies = retained
            .iter()
            .filter(|event| declared_body_unavailable(event))
            .count();
        let verifiable_bodies = retained
            .iter()
            .filter(|event| body_requires_verification(event))
            .count();
        let mut verified_bodies = 0usize;
        let capture_gaps = retained
            .iter()
            .filter(|event| event.event.kind == CapturedEventKindV1::Gap)
            .count();
        let mut events = Vec::new();
        let mut next_offset = offset;
        let mut encoded_bytes = 0usize;
        for retained in retained.iter().skip(offset).take(limit) {
            let body = self.body(retained);
            let event = ConversationEventV1 {
                work_id: retained.work_id.clone(),
                generation: retained.generation,
                session_id: retained.session_id.clone(),
                event_id: retained.event.event_id.clone(),
                sequence: retained.event.sequence,
                observed_at: retained.event.observed_at,
                kind: retained.event.kind,
                tool: retained.event.tool.clone(),
                relation: EvidenceRelationV1::SameWorkSession,
                body,
                gap: retained.event.gap.clone(),
                retention: retained_state(retained.state),
            };
            let event_bytes = serde_json::to_vec(&event)?.len();
            if !events.is_empty()
                && encoded_bytes.saturating_add(event_bytes) > MAX_CONVERSATION_PAGE_BYTES
            {
                break;
            }
            if body_requires_verification(retained) {
                verified_bodies = verified_bodies.saturating_add(1);
                if event.body.availability != BodyAvailabilityV1::Available {
                    unavailable_bodies = unavailable_bodies.saturating_add(1);
                }
            }
            encoded_bytes = encoded_bytes.saturating_add(event_bytes);
            events.push(event);
            next_offset = next_offset.saturating_add(1);
        }
        let next_cursor =
            (next_offset < retained.len()).then(|| encode_cursor("c", &query, next_offset));
        let unverified_bodies = verifiable_bodies.saturating_sub(verified_bodies);
        let projection_coverage = coverage(
            &projection,
            unavailable_bodies,
            unverified_bodies,
            capture_gaps,
        );
        let complete = next_cursor.is_none();
        Ok(ConversationPageV1 {
            version: 1,
            repository: self.repository.clone(),
            work_id: work_id.to_owned(),
            session_id: session_id.to_owned(),
            relation: EvidenceRelationV1::SameWorkSession,
            events,
            projection_coverage,
            next_cursor,
            complete,
        })
    }

    fn body(&self, retained: &RetainedEvent) -> ConversationBodyV1 {
        if !body_applicable(retained.event.kind) {
            return unavailable_body(BodyAvailabilityV1::NotApplicable, None);
        }
        let Some(reference) = &retained.event.artifact else {
            return unavailable_body(BodyAvailabilityV1::NotCaptured, None);
        };
        let declared = match reference.availability {
            ContentAvailabilityV1::Local => None,
            ContentAvailabilityV1::Missing => Some(BodyAvailabilityV1::Missing),
            ContentAvailabilityV1::Expired => Some(BodyAvailabilityV1::Expired),
            ContentAvailabilityV1::Deleted => Some(BodyAvailabilityV1::Deleted),
            ContentAvailabilityV1::Denied => Some(BodyAvailabilityV1::Denied),
            ContentAvailabilityV1::IntegrityFailure => Some(BodyAvailabilityV1::IntegrityFailure),
            ContentAvailabilityV1::Shared | ContentAvailabilityV1::External => {
                Some(BodyAvailabilityV1::Denied)
            }
            ContentAvailabilityV1::Mixed => Some(BodyAvailabilityV1::Missing),
        };
        if let Some(availability) = declared {
            return unavailable_body(availability, Some(reference.media));
        }
        let bytes = match self.spool.read_artifact(reference, MAX_BODY_ARTIFACT_BYTES) {
            Ok(bytes) => bytes,
            Err(CaptureError::Limit(_)) => {
                return unavailable_body(BodyAvailabilityV1::TooLarge, Some(reference.media));
            }
            Err(CaptureError::NotFound(_)) => {
                return unavailable_body(BodyAvailabilityV1::Missing, Some(reference.media));
            }
            Err(CaptureError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return unavailable_body(BodyAvailabilityV1::Missing, Some(reference.media));
            }
            Err(CaptureError::Conflict(_)) => {
                return unavailable_body(
                    BodyAvailabilityV1::IntegrityFailure,
                    Some(reference.media),
                );
            }
            Err(_) => {
                return unavailable_body(BodyAvailabilityV1::Denied, Some(reference.media));
            }
        };
        match render_body(reference.media, retained.event.kind, &bytes) {
            Some(content) => ConversationBodyV1 {
                availability: BodyAvailabilityV1::Available,
                media: Some(reference.media),
                content: Some(content),
            },
            None => unavailable_body(BodyAvailabilityV1::InvalidBody, Some(reference.media)),
        }
    }
}

type ActionKey = (String, i64, String);

fn retained_actions(
    projection: &ReadOnlySpoolProjection,
) -> Result<(Vec<RetainedAction>, Vec<ActionKey>)> {
    let mut starts = BTreeMap::<ActionKey, (ActionStartV1, ReadOnlyEnvelopeState)>::new();
    let mut finishes = BTreeMap::<ActionKey, (ActionFinishV1, ReadOnlyEnvelopeState)>::new();
    for retained in &projection.envelopes {
        match &retained.envelope {
            CaptureEnvelopeV1::ActionStart(start) => {
                let key = (
                    start.work_id.clone(),
                    start.generation,
                    start.action_id.clone(),
                );
                if starts
                    .insert(key, (start.clone(), retained.state))
                    .is_some()
                {
                    return Err(Error::Conflict(
                        "duplicate retained action start identity".into(),
                    ));
                }
            }
            CaptureEnvelopeV1::ActionFinish(finish) => {
                let key = (
                    finish.work_id.clone(),
                    finish.generation,
                    finish.action_id.clone(),
                );
                if finishes
                    .insert(key, (finish.clone(), retained.state))
                    .is_some()
                {
                    return Err(Error::Conflict(
                        "duplicate retained action finish identity".into(),
                    ));
                }
            }
            CaptureEnvelopeV1::EventBatch(_) | CaptureEnvelopeV1::Usage(_) => {}
        }
    }
    let mut actions = Vec::new();
    for (key, (start, state)) in starts {
        let finish = finishes.remove(&key);
        actions.push(RetainedAction {
            start,
            start_state: state,
            finish,
        });
    }
    let finish_only = finishes.into_keys().collect();
    actions.sort_by(|left, right| {
        (
            left.start.started_at,
            left.start.work_id.as_str(),
            left.start.generation,
            left.start.action_id.as_str(),
        )
            .cmp(&(
                right.start.started_at,
                right.start.work_id.as_str(),
                right.start.generation,
                right.start.action_id.as_str(),
            ))
    });
    Ok((actions, finish_only))
}

fn retained_events(projection: &ReadOnlySpoolProjection) -> Result<Vec<RetainedEvent>> {
    let mut events = BTreeMap::<(String, i64, String, u64, String), RetainedEvent>::new();
    for retained in &projection.envelopes {
        let CaptureEnvelopeV1::EventBatch(batch) = &retained.envelope else {
            continue;
        };
        for event in &batch.events {
            let key = (
                batch.work_id.clone(),
                batch.generation,
                batch.session.session_id.clone(),
                event.sequence,
                event.event_id.clone(),
            );
            let value = RetainedEvent {
                work_id: batch.work_id.clone(),
                generation: batch.generation,
                session_id: batch.session.session_id.clone(),
                capture_mode: batch.session.capture_mode,
                event: event.clone(),
                state: retained.state,
            };
            if let Some(previous) = events.insert(key, value.clone())
                && (previous.event != value.event
                    || previous.capture_mode != value.capture_mode
                    || previous.state != value.state)
            {
                return Err(Error::Conflict(
                    "retained conversation event identity has conflicting facts".into(),
                ));
            }
        }
    }
    Ok(events.into_values().collect())
}

fn retained_sessions(projection: &ReadOnlySpoolProjection) -> Result<Vec<SessionAggregate>> {
    type Key = (String, i64, String);
    let mut sessions = BTreeMap::<Key, SessionAggregate>::new();
    for retained in &projection.envelopes {
        let CaptureEnvelopeV1::EventBatch(batch) = &retained.envelope else {
            continue;
        };
        // Batches need not be ordered by time, so take the extremes rather than the ends.
        let (Some(first_at), Some(last_at)) = (
            batch.events.iter().map(|event| event.observed_at).min(),
            batch.events.iter().map(|event| event.observed_at).max(),
        ) else {
            continue;
        };
        let key = (
            batch.work_id.clone(),
            batch.generation,
            batch.session.session_id.clone(),
        );
        let unavailable = batch
            .events
            .iter()
            .filter(|event| {
                body_applicable(event.kind)
                    && event.artifact.as_ref().is_none_or(|artifact| {
                        artifact.availability != ContentAvailabilityV1::Local
                            || artifact.size_bytes > MAX_BODY_ARTIFACT_BYTES
                    })
            })
            .count();
        let unverified = batch
            .events
            .iter()
            .filter(|event| body_requires_verification_parts(event.kind, event.artifact.as_ref()))
            .count();
        let gaps = batch
            .events
            .iter()
            .filter(|event| event.kind == CapturedEventKindV1::Gap)
            .count();
        match sessions.get_mut(&key) {
            Some(session) => {
                if session.harness != batch.session.harness
                    || session.capture_mode != batch.session.capture_mode
                {
                    return Err(Error::Conflict(
                        "retained session identity has conflicting capture facts".into(),
                    ));
                }
                session.first_observed_at = session.first_observed_at.min(first_at);
                session.last_observed_at = session.last_observed_at.max(last_at);
                session.event_count = session.event_count.saturating_add(batch.events.len());
                session.unavailable_body_count =
                    session.unavailable_body_count.saturating_add(unavailable);
                session.unverified_body_count =
                    session.unverified_body_count.saturating_add(unverified);
                session.capture_gap_count = session.capture_gap_count.saturating_add(gaps);
            }
            None => {
                sessions.insert(
                    key,
                    SessionAggregate {
                        work_id: batch.work_id.clone(),
                        generation: batch.generation,
                        session_id: batch.session.session_id.clone(),
                        harness: batch.session.harness,
                        capture_mode: batch.session.capture_mode,
                        first_observed_at: first_at,
                        last_observed_at: last_at,
                        event_count: batch.events.len(),
                        unavailable_body_count: unavailable,
                        unverified_body_count: unverified,
                        capture_gap_count: gaps,
                    },
                );
            }
        }
    }
    Ok(sessions.into_values().collect())
}

fn action_evidence(action: RetainedAction, commit: &str, path_requested: bool) -> ActionEvidenceV1 {
    let applicability = match &action.start.source {
        SourceBindingV1::Git {
            head,
            dirty,
            untracked_count,
            omissions,
            ..
        } if head == commit && !dirty && *untracked_count == 0 && omissions.is_empty() => {
            EvidenceApplicabilityV1::MatchingRecordedCleanSource
        }
        SourceBindingV1::Git { .. } => EvidenceApplicabilityV1::DifferentRecordedSource,
        SourceBindingV1::Native { .. } => EvidenceApplicabilityV1::Unknown,
    };
    let result = action.finish.map(|(finish, state)| ActionResultEvidenceV1 {
        operation_id: finish.operation_id,
        outcome: finish.outcome,
        result: finish.result,
        assurance: finish.assurance,
        coverage: finish.coverage,
        drift: finish.drift,
        finished_at: finish.finished_at,
        retention: retained_state(state),
    });
    ActionEvidenceV1 {
        relation: EvidenceRelationV1::ExactSourceBinding,
        path_relation: if path_requested {
            PathRelationV1::NotRecorded
        } else {
            PathRelationV1::NotRequested
        },
        work_id: action.start.work_id,
        generation: action.start.generation,
        session_id: action.start.session_id,
        action_id: action.start.action_id,
        operation_id: action.start.operation_id,
        action_kind: action.start.kind,
        recipe_provenance: action.start.recipe_provenance,
        recipe_digest: action.start.recipe_digest,
        setup_digest: action.start.setup_digest,
        started_at: action.start.started_at,
        source: action.start.source,
        applicability,
        start_retention: retained_state(action.start_state),
        result,
    }
}

fn retained_state(state: ReadOnlyEnvelopeState) -> RetainedEnvelopeStateV1 {
    match state {
        ReadOnlyEnvelopeState::Pending => RetainedEnvelopeStateV1::Pending,
        ReadOnlyEnvelopeState::Acknowledged => RetainedEnvelopeStateV1::Acknowledged,
        ReadOnlyEnvelopeState::Compacted => RetainedEnvelopeStateV1::Compacted,
    }
}

fn coverage(
    projection: &ReadOnlySpoolProjection,
    unavailable_bodies: usize,
    unverified_bodies: usize,
    capture_gaps: usize,
) -> ProjectionCoverageV1 {
    ProjectionCoverageV1 {
        retained_envelopes: projection.envelopes.len(),
        compacted_event_batches_unattributed: projection.compacted_event_batches_unattributed,
        unavailable_bodies,
        unverified_bodies,
        capture_gaps,
        complete: projection.compacted_event_batches_unattributed == 0
            && unavailable_bodies == 0
            && unverified_bodies == 0
            && capture_gaps == 0,
    }
}

fn evidence_entry_key(entry: &EvidenceEntryV1) -> (i64, &str, i64, &str, u8) {
    match entry {
        EvidenceEntryV1::Action(action) => (
            action.started_at,
            action.work_id.as_str(),
            action.generation,
            action.action_id.as_str(),
            0,
        ),
        EvidenceEntryV1::Session(session) => (
            session.first_observed_at,
            session.work_id.as_str(),
            session.generation,
            session.session_id.as_str(),
            1,
        ),
    }
}

fn projection_fingerprint(projection: &ReadOnlySpoolProjection) -> Result<String> {
    let mut digest = Sha256::new();
    digest.update(
        projection
            .compacted_event_batches_unattributed
            .to_string()
            .as_bytes(),
    );
    for retained in &projection.envelopes {
        digest.update([match retained.state {
            ReadOnlyEnvelopeState::Pending => 0,
            ReadOnlyEnvelopeState::Acknowledged => 1,
            ReadOnlyEnvelopeState::Compacted => 2,
        }]);
        digest.update(serde_json::to_vec(&retained.envelope)?);
        digest.update([0]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn query_digest(parts: &[&str]) -> String {
    let mut bytes = Vec::new();
    for part in parts {
        bytes.extend_from_slice(part.len().to_string().as_bytes());
        bytes.push(b':');
        bytes.extend_from_slice(part.as_bytes());
        bytes.push(0);
    }
    sha256(&bytes)
}

fn encode_cursor(kind: &str, query: &str, offset: usize) -> String {
    format!("x1:{kind}:{query}:{offset:x}")
}

fn decode_cursor(cursor: Option<&str>, kind: &str, query: &str) -> Result<usize> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    if cursor.is_empty() || cursor.len() > MAX_CURSOR_BYTES {
        return Err(Error::Invalid("explorer cursor is invalid".into()));
    }
    let mut parts = cursor.split(':');
    if parts.next() != Some("x1") || parts.next() != Some(kind) || parts.next() != Some(query) {
        return Err(Error::Conflict(
            "explorer projection changed; refresh without the prior cursor".into(),
        ));
    }
    let offset = parts
        .next()
        .filter(|value| !value.is_empty() && value.len() <= 16)
        .and_then(|value| usize::from_str_radix(value, 16).ok())
        .ok_or_else(|| Error::Invalid("explorer cursor is invalid".into()))?;
    if parts.next().is_some() {
        return Err(Error::Invalid("explorer cursor is invalid".into()));
    }
    Ok(offset)
}

fn page<T>(
    values: Vec<T>,
    offset: usize,
    limit: usize,
    kind: &str,
    query: &str,
) -> Result<(Vec<T>, Option<String>)> {
    if offset > values.len() {
        return Err(Error::Invalid("explorer cursor is out of range".into()));
    }
    let end = offset.saturating_add(limit).min(values.len());
    let next = (end < values.len()).then(|| encode_cursor(kind, query, end));
    Ok((values.into_iter().skip(offset).take(limit).collect(), next))
}

fn validate_commit(commit: &str) -> Result<()> {
    if !matches!(commit.len(), 40 | 64)
        || !commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(
            "explorer evidence requires a full lower-case commit id".into(),
        ));
    }
    Ok(())
}

fn validate_limit(limit: usize) -> Result<()> {
    if !(1..=MAX_PAGE_LIMIT).contains(&limit) {
        return Err(Error::Invalid("explorer page limit must be 1..100".into()));
    }
    Ok(())
}

fn validate_identifier(label: &str, value: &str) -> Result<()> {
    if !bounded_identifier(value, 255) {
        return Err(Error::Invalid(format!("explorer {label} is invalid")));
    }
    Ok(())
}

fn bounded_identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'@')
        })
}

fn bounded_repository(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn validate_repository_path(path: &str) -> Result<String> {
    if path.is_empty() || path.len() > 4_096 || path.contains('\0') {
        return Err(Error::Invalid("explorer repository path is invalid".into()));
    }
    let path_value = Path::new(path);
    if path_value.is_absolute()
        || path_value.components().any(|component| {
            !matches!(component, Component::Normal(_))
                || matches!(component, Component::ParentDir | Component::RootDir)
        })
    {
        return Err(Error::Invalid("explorer repository path is invalid".into()));
    }
    Ok(path.to_owned())
}

fn declared_body_unavailable(retained: &RetainedEvent) -> bool {
    if !body_applicable(retained.event.kind) {
        return false;
    }
    retained.event.artifact.as_ref().is_none_or(|artifact| {
        artifact.availability != ContentAvailabilityV1::Local
            || artifact.size_bytes > MAX_BODY_ARTIFACT_BYTES
    })
}

fn body_requires_verification(retained: &RetainedEvent) -> bool {
    body_requires_verification_parts(retained.event.kind, retained.event.artifact.as_ref())
}

fn body_requires_verification_parts(
    kind: CapturedEventKindV1,
    artifact: Option<&crate::change_record::ArtifactRefV1>,
) -> bool {
    body_applicable(kind)
        && artifact.is_some_and(|artifact| {
            artifact.availability == ContentAvailabilityV1::Local
                && artifact.size_bytes <= MAX_BODY_ARTIFACT_BYTES
        })
}

fn body_applicable(kind: CapturedEventKindV1) -> bool {
    matches!(
        kind,
        CapturedEventKindV1::UserPrompt
            | CapturedEventKindV1::AssistantMessage
            | CapturedEventKindV1::ToolStarted
            | CapturedEventKindV1::ToolFinished
            | CapturedEventKindV1::CheckStarted
            | CapturedEventKindV1::CheckFinished
    )
}

fn unavailable_body(
    availability: BodyAvailabilityV1,
    media: Option<ArtifactMediaV1>,
) -> ConversationBodyV1 {
    ConversationBodyV1 {
        availability,
        media,
        content: None,
    }
}

fn render_body(media: ArtifactMediaV1, kind: CapturedEventKindV1, bytes: &[u8]) -> Option<String> {
    match media {
        ArtifactMediaV1::Json => {
            let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
            if matches!(
                kind,
                CapturedEventKindV1::UserPrompt | CapturedEventKindV1::AssistantMessage
            ) && let Some(content) = value.get("content").and_then(serde_json::Value::as_str)
            {
                return Some(content.to_owned());
            }
            serde_json::to_string(&value).ok()
        }
        ArtifactMediaV1::Text | ArtifactMediaV1::JsonLines => {
            std::str::from_utf8(bytes).ok().map(str::to_owned)
        }
        ArtifactMediaV1::Binary => None,
    }
}

fn map_capture_error(error: CaptureError) -> Error {
    match error {
        CaptureError::Invalid(message) => Error::Invalid(message),
        CaptureError::Conflict(message) => Error::Conflict(message),
        CaptureError::Limit(message) => Error::Limit(message),
        CaptureError::NotFound(message) => Error::NotFound(message),
        CaptureError::Io(error) => Error::Io(error),
        CaptureError::Json(error) => Error::Json(error),
    }
}
