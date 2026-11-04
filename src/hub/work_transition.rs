//! Store-level Work transitions: the request, who is acting, and the rules both stores share.
//!
//! The matrix itself is [`super::work_lifecycle::decide_transition`]. This module turns a caller
//! into an actor, validates a request, applies the transition to a Work item and builds the
//! audit event, so the SQLite and PostgreSQL stores differ only in how they read and write rows.
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::intent::text;
use super::store::{Event, User};
use super::work::{
    CheckpointReceipt, ClaimStatus, WorkItem, WorkParticipant, WorkRole, WorkStartConflict,
    WorkState, WorkWho, validate_id,
};
pub use super::work_lifecycle::{
    ActorVia, OperationalOverviewV1, OutcomeProvenance, ReservationBasis, TerminalOutcome,
    WorkActorKind, WorkOperationalV1, WorkTransitionKind,
};
use super::work_lifecycle::{Refusal, decide_transition};
use super::{HubError, Result};

const MAX_REASON_BYTES: usize = 500;
const MAX_LABEL_BYTES: usize = 255;
const MAX_PARTICIPANTS: usize = 100;
const MAX_RELATIONS: usize = 100;

/// Who asks for a transition. The caller establishes a person's identity and administrator flag;
/// the store derives lead, owner or admin from the current item inside its transaction.
#[derive(Clone, Copy, Debug)]
pub enum WorkTransitionActor<'a> {
    /// An agent token. Needs a named session in the request.
    Agent { token: &'a str },
    /// A signed-in person on one connected repository whose read role the caller has checked.
    /// No agent token is required or consulted.
    Person {
        user: &'a User,
        repository: &'a str,
        repository_id: i64,
        is_admin: bool,
    },
}

/// Whose view a lifecycle read is.
#[derive(Clone, Copy, Debug)]
pub enum WorkLifecycleReader<'a> {
    Token(&'a str),
    Person {
        owner: &'a User,
        repository: &'a str,
        repository_id: i64,
    },
}

/// Which Work an operational read covers.
#[derive(Clone, Copy, Debug)]
pub enum OperationalSelect<'a> {
    /// One Work item; the page is empty when it is not in the reader's repository.
    One(&'a str),
    /// These Work items, whether or not they hold a reservation.
    Many(&'a [String]),
    /// Every Work item of the repository that holds a reservation, including Work hidden by a
    /// merged pull request.
    Holders,
}

/// The most Work ids one `Many` read accepts.
pub const MAX_OPERATIONAL_IDS: usize = 100;

impl OperationalSelect<'_> {
    /// The ids to read, validated, or None for the reservation holders.
    pub(super) fn ids(&self) -> Result<Option<Vec<String>>> {
        let ids: Vec<String> = match self {
            Self::One(id) => vec![(*id).to_owned()],
            Self::Many(ids) => ids.to_vec(),
            Self::Holders => return Ok(None),
        };
        if ids.len() > MAX_OPERATIONAL_IDS {
            return Err(invalid("more than 100 work ids"));
        }
        for id in &ids {
            validate_id("work", id, "wrk")?;
        }
        let mut ids = ids;
        ids.sort();
        ids.dedup();
        Ok(Some(ids))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTransitionRequest {
    pub work_id: String,
    /// Stable caller-chosen idempotency key.
    pub operation_id: String,
    pub generation: i64,
    pub expected_updated_seq: i64,
    /// The acting agent session. Required for an agent actor, absent for a person.
    #[serde(default)]
    pub session: Option<String>,
    pub transition: WorkTransitionKind,
    pub reason: String,
    /// The Work that replaces this one; required for `supersede` and refused otherwise.
    #[serde(default)]
    pub superseded_by: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkTransitionResult {
    /// The current authorized projection, which can be newer than a replayed operation.
    pub item: WorkItem,
    /// Absent when a reopen found a scope overlap and changed nothing.
    pub receipt: Option<CheckpointReceipt>,
    pub conflict: Option<WorkStartConflict>,
}

impl WorkTransitionRequest {
    pub(super) fn validate(&self) -> Result<()> {
        validate_id("work", &self.work_id, "wrk")?;
        text("operation_id", &self.operation_id, MAX_LABEL_BYTES)?;
        if self.generation < 1 {
            return Err(invalid("generation must be positive"));
        }
        if self.expected_updated_seq < 1 {
            return Err(invalid("expected_updated_seq must be positive"));
        }
        if let Some(session) = &self.session {
            text("session", session, MAX_LABEL_BYTES)?;
        }
        text("reason", &self.reason, MAX_REASON_BYTES)?;
        match (&self.transition, &self.superseded_by) {
            (WorkTransitionKind::Supersede, Some(by)) => {
                validate_id("superseded_by", by, "wrk")?;
                if by == &self.work_id {
                    return Err(invalid("work cannot supersede itself"));
                }
            }
            (WorkTransitionKind::Supersede, None) => {
                return Err(invalid("supersede requires superseded_by"));
            }
            (_, Some(_)) => {
                return Err(invalid("superseded_by is only valid for supersede"));
            }
            _ => {}
        }
        Ok(())
    }

    /// The exact-retry identity: every request field and who asked, never the actor's role.
    pub(super) fn envelope(&self, via: ActorVia, user: &str) -> Result<String> {
        serde_json::to_string(&json!({
            "operation": "work_transition",
            "request": self,
            "actor": {"via": via, "user": user},
        }))
        .map_err(|error| HubError::Unavailable(format!("encode transition request: {error}")))
    }
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.to_owned())
}

/// The scope a person acts in. Reads need no token, so none is held.
pub(super) fn person_who(user: &User, repository: &str, repository_id: i64) -> WorkWho {
    WorkWho {
        token: String::new(),
        repository: repository.to_owned(),
        repository_id: Some(repository_id),
        agent: String::new(),
        read_only: false,
        owner: user.clone(),
    }
}

/// The identity of whoever acts, resolved against the current item.
pub(super) struct Acting {
    pub kind: WorkActorKind,
    pub via: ActorVia,
    pub user_id: String,
    pub login: String,
    /// The agent's token, name and session, absent for a person.
    pub agent: Option<(String, String, String)>,
}

/// Derives the actor kind from the current item, or None when the caller has no standing.
/// A lead is checked first, so a lead that is also the owner acts as the lead.
pub(super) fn classify(
    item: &WorkItem,
    who: &WorkWho,
    actor: &WorkTransitionActor<'_>,
    session: Option<&str>,
) -> Result<Option<Acting>> {
    match actor {
        WorkTransitionActor::Agent { .. } => {
            if who.read_only {
                return Err(HubError::Unauthorized);
            }
            let session = session.ok_or_else(|| invalid("session is required for an agent"))?;
            let kind = if item.lead_token == who.token && item.lead_session == session {
                WorkActorKind::Lead
            } else if who.owner.id == item.owner {
                WorkActorKind::Owner
            } else {
                return Ok(None);
            };
            Ok(Some(Acting {
                kind,
                via: ActorVia::Agent,
                user_id: who.owner.id.clone(),
                login: who.owner.login.clone(),
                agent: Some((who.token.clone(), who.agent.clone(), session.to_owned())),
            }))
        }
        WorkTransitionActor::Person { user, is_admin, .. } => {
            if session.is_some() {
                return Err(invalid("a person does not act through a session"));
            }
            let kind = if user.id == item.owner {
                WorkActorKind::Owner
            } else if *is_admin {
                WorkActorKind::Admin
            } else {
                return Ok(None);
            };
            Ok(Some(Acting {
                kind,
                via: ActorVia::Person,
                user_id: user.id.clone(),
                login: user.login.clone(),
                agent: None,
            }))
        }
    }
}

/// Applies the matrix and the generation and sequence fences, in that order.
pub(super) fn check(
    item: &WorkItem,
    held: bool,
    acting: &Acting,
    request: &WorkTransitionRequest,
) -> Result<()> {
    decide_transition(
        request.transition,
        acting.kind,
        acting.via,
        item.state,
        held,
    )
    .map_err(|refusal| {
        invalid(match refusal {
            Refusal::NotAuthorized => "this actor may not request this transition",
            Refusal::WrongState => "the Work state does not allow this transition",
            Refusal::NoReservation => "the Work holds no reservation",
        })
    })?;
    if item.generation != request.generation {
        return Err(HubError::Invalid(format!(
            "transition generation is fenced; current generation is {}",
            item.generation
        )));
    }
    if item.updated_seq != request.expected_updated_seq {
        return Err(HubError::Invalid(format!(
            "transition receipt expired or work changed; current updated_seq is {}",
            item.updated_seq
        )));
    }
    Ok(())
}

/// What the item was before the transition, for the audit event.
pub(super) struct Prior {
    pub state: WorkState,
    pub basis: Option<ReservationBasis>,
    pub outcome: Option<TerminalOutcome>,
    pub generation: i64,
    pub lead_token: String,
    pub lead_agent: String,
    pub lead_session: String,
}

impl Prior {
    /// Whether the item now has another lead session than before the transition.
    pub(super) fn lead_changed(&self, item: &WorkItem) -> bool {
        self.lead_token != item.lead_token || self.lead_session != item.lead_session
    }
}

/// The reservation columns after the transition.
pub(super) struct Columns {
    pub outcome: Option<TerminalOutcome>,
    pub basis: Option<ReservationBasis>,
}

/// Changes the item in memory. `current` and `blocker` are never touched. Reopen makes the
/// caller the lead; the previous lead becomes a helper.
pub(super) fn apply(
    item: &mut WorkItem,
    request: &WorkTransitionRequest,
    acting: &Acting,
    now: i64,
) -> Result<Columns> {
    use WorkTransitionKind::{Abandon, Complete, Release, Reopen, Retain, Supersede};
    let lead = acting.kind == WorkActorKind::Lead;
    item.updated_at = now;
    Ok(match request.transition {
        Release => {
            item.generation += 1;
            if item.state == WorkState::Active {
                item.state = WorkState::Paused;
            }
            Columns {
                outcome: None,
                basis: None,
            }
        }
        Retain => Columns {
            outcome: None,
            basis: Some(ReservationBasis::Retained),
        },
        Complete | Abandon | Supersede => {
            if let Some(by) = &request.superseded_by
                && !item.depends_on.contains(by)
                && !item.related_work.contains(by)
            {
                if item.depends_on.len() + item.related_work.len() >= MAX_RELATIONS {
                    return Err(invalid("work has more than 100 relations"));
                }
                item.related_work.push(by.clone());
            }
            if !lead {
                item.generation += 1;
            }
            item.state = WorkState::Finished;
            Columns {
                outcome: Some(match request.transition {
                    Complete => TerminalOutcome::Completed,
                    Abandon => TerminalOutcome::Abandoned,
                    _ => TerminalOutcome::Superseded,
                }),
                basis: None,
            }
        }
        Reopen => {
            let (token, agent, session) = acting
                .agent
                .as_ref()
                .ok_or_else(|| invalid("only an agent session can reopen Work"))?;
            let known = item
                .participants
                .iter()
                .any(|participant| participant.token == *token && participant.session == *session);
            if !known && item.participants.len() >= MAX_PARTICIPANTS {
                return Err(invalid("work has the maximum of 100 participants"));
            }
            for participant in &mut item.participants {
                if participant.role == WorkRole::Lead {
                    participant.role = WorkRole::Helper;
                }
            }
            item.participants
                .retain(|p| p.token != *token || p.session != *session);
            item.participants.push(WorkParticipant {
                owner: acting.user_id.clone(),
                login: acting.login.clone(),
                token: token.clone(),
                agent: agent.clone(),
                session: session.clone(),
                role: WorkRole::Lead,
            });
            item.participants.sort_by(|a, b| {
                (&a.owner, &a.token, &a.session).cmp(&(&b.owner, &b.token, &b.session))
            });
            item.lead_token = token.clone();
            item.lead_agent = agent.clone();
            item.lead_session = session.clone();
            item.generation += 1;
            item.state = WorkState::Active;
            item.last_checkin_at = now;
            item.reported_at = None;
            item.claim_status = ClaimStatus::Active;
            Columns {
                outcome: None,
                basis: Some(ReservationBasis::Admitted),
            }
        }
    })
}

/// The `work_transition` audit event. It names the actor, never a token, and for an owner or
/// administrator the lead it acted on.
pub(super) fn event(
    item: &WorkItem,
    request: &WorkTransitionRequest,
    acting: &Acting,
    prior: &Prior,
    columns: &Columns,
) -> Result<Event> {
    let lead_acted_on = (acting.kind != WorkActorKind::Lead)
        .then(|| json!({"agent": prior.lead_agent, "session": prior.lead_session}));
    let mut event = Event::new(
        "work_transition",
        &item.id,
        json!({
            "transition": request.transition,
            "actor": {"kind": acting.kind, "id": acting.user_id,
                      "login": acting.login, "via": acting.via},
            "reason": request.reason,
            "operation_id": request.operation_id,
            "prior_state": prior.state,
            "prior_basis": prior.basis,
            "prior_outcome": prior.outcome,
            "prior_generation": prior.generation,
            "outcome": columns.outcome,
            "basis": columns.basis,
            "superseded_by": request.superseded_by,
            "lead": lead_acted_on,
            "generation": item.generation, "owner": item.owner,
            "repository": item.repository, "state": item.state,
        }),
    )?;
    event.at = item.updated_at;
    Ok(event)
}

/// Why Work stopped holding its scope: what the audit event of an affected request records.
pub(super) struct ReleaseCause {
    transition: &'static str,
    actor: serde_json::Value,
}

impl ReleaseCause {
    pub(super) fn transition(request: &WorkTransitionRequest, acting: &Acting) -> Self {
        Self {
            transition: request.transition.as_str(),
            actor: json!({"kind": acting.kind, "id": acting.user_id, "via": acting.via}),
        }
    }

    /// A `finish` checkpoint by the lead session of an agent token owned by `owner`.
    pub(super) fn finish_checkpoint(owner: &str) -> Self {
        Self {
            transition: "finish",
            actor: json!({"kind": WorkActorKind::Lead, "id": owner, "via": ActorVia::Agent}),
        }
    }
}

/// The `coordination_target_released` event for one open request. The request keeps its status.
pub(super) fn released_target_event(
    request_id: &str,
    item: &WorkItem,
    cause: &ReleaseCause,
) -> Result<Event> {
    let mut event = Event::new(
        "coordination_target_released",
        request_id,
        json!({"work_id": item.id, "transition": cause.transition,
            "actor": cause.actor, "generation": item.generation}),
    )?;
    event.at = item.updated_at;
    Ok(event)
}

/// The basis a pause or handover checkpoint leaves behind. Only a basis that was never an
/// explicit decision becomes implicit; a retained reservation and an implicit one stay as they are.
pub(super) fn basis_after_pause(prior: Option<ReservationBasis>) -> Option<ReservationBasis> {
    match prior {
        Some(ReservationBasis::Admitted) => Some(ReservationBasis::LegacyImplicit),
        other => other,
    }
}

/// The refusal for a checkpoint that reports work on Work that holds nothing.
pub(super) fn needs_resume(kind: super::work::CheckpointKind) -> Result<()> {
    use super::work::CheckpointKind::{Blocker, Decision, Progress};
    if matches!(kind, Progress | Decision | Blocker) {
        return Err(invalid(
            "this work holds no reservation; resume it with a takeover start before reporting progress, a decision or a blocker",
        ));
    }
    Ok(())
}

/// Whether a no-session intent announce may proceed for a token, given the scopes of the Work its
/// sessions are currently eligible on and whether any session was ever linked to Work here.
/// A token never linked to Work keeps the legacy advice.
pub(super) fn check_announce(
    linked: bool,
    eligible_scopes: &[Vec<String>],
    keys: &[String],
) -> Result<()> {
    if !linked {
        return Ok(());
    }
    let mut uncovered = keys.to_vec();
    for scope in eligible_scopes {
        uncovered = super::work::uncovered_scope(scope, &uncovered);
    }
    if eligible_scopes.is_empty() || !uncovered.is_empty() {
        return Err(invalid(
            "These files are not covered by a current unfinished assignment. Start or explicitly rejoin the work, or checkpoint the scope before editing.",
        ));
    }
    Ok(())
}

/// Whether the transition frees the lead's and participants' file claims and open requests.
/// These are cleanup, so they record their size without a quota check.
pub(super) fn releases_claims(kind: WorkTransitionKind) -> bool {
    matches!(
        kind,
        WorkTransitionKind::Release
            | WorkTransitionKind::Complete
            | WorkTransitionKind::Abandon
            | WorkTransitionKind::Supersede
    )
}

/// Every session that may hold a live intent for the item: the lead and each participant.
pub(super) fn claim_sessions(item: &WorkItem) -> Vec<(String, String)> {
    let mut sessions: Vec<(String, String)> = item
        .participants
        .iter()
        .map(|participant| (participant.token.clone(), participant.session.clone()))
        .collect();
    sessions.push((item.lead_token.clone(), item.lead_session.clone()));
    sessions.sort();
    sessions.dedup();
    sessions
}

/// Who stated a terminal outcome, from the latest terminal event of the Work.
pub(super) fn provenance_from_event(kind: &str, data: &str) -> OutcomeProvenance {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(data) else {
        return OutcomeProvenance::Unknown;
    };
    match kind {
        // A finish checkpoint is the lead agent's own report.
        "work_checkpoint" if value["state"] == "finished" => OutcomeProvenance::AgentReport,
        "work_transition"
            if matches!(
                value["transition"].as_str(),
                Some("complete" | "abandon" | "supersede")
            ) =>
        {
            match (
                value["actor"]["via"].as_str(),
                value["actor"]["kind"].as_str(),
            ) {
                (Some("agent"), _) => OutcomeProvenance::AgentReport,
                (Some("person"), Some("admin")) => OutcomeProvenance::Admin,
                (Some("person"), _) => OutcomeProvenance::Person,
                _ => OutcomeProvenance::Unknown,
            }
        }
        _ => OutcomeProvenance::Unknown,
    }
}
