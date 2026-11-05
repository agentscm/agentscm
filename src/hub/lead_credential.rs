//! The same owner and the same session keep the Work lead when the presented
//! token or agent label changes. A different owner or a different session stays
//! refused. The generation does not move: this is not a takeover.

use super::work::{WorkItem, WorkParticipant, WorkRole, WorkState};

/// Stable refusal. HTTP and MCP pass a message that is this code, or that
/// starts with it, through the way they pass `SESSION_REQUIRED`.
pub(super) const LEAD_SESSION_MISMATCH: &str = "LEAD_SESSION_MISMATCH";

/// Audit event for a credential carry. The payload names the session and the
/// agent labels. It does not name a token.
pub(super) const KIND: &str = "work_lead_credential";

pub(super) enum LeadAccess {
    /// The presented token, agent, and session are the recorded lead.
    Current,
    /// Same owner, same session, live Work, and a different token or agent.
    Carry,
    Refuse {
        message: String,
    },
}

pub(super) fn lead_access(
    item: &WorkItem,
    owner_id: &str,
    token: &str,
    agent: &str,
    session: &str,
) -> LeadAccess {
    if item.lead_token == token && item.lead_agent == agent && item.lead_session == session {
        return LeadAccess::Current;
    }
    if item.owner == owner_id
        && item.lead_session == session
        && item.state != WorkState::Finished
        && (item.lead_token != token || item.lead_agent != agent)
    {
        return LeadAccess::Carry;
    }
    LeadAccess::Refuse {
        message: mismatch_message(item, owner_id),
    }
}

fn mismatch_message(item: &WorkItem, owner_id: &str) -> String {
    if item.owner != owner_id {
        return LEAD_SESSION_MISMATCH.to_owned();
    }
    format!(
        "{LEAD_SESSION_MISMATCH}: recorded lead agent {agent}, session {session}. Take over with agentscm_start work_id {id} takeover_generation {generation}.",
        agent = super::advice::agent_name(&item.lead_agent),
        session = item.lead_session,
        id = item.id,
        generation = item.generation,
    )
}

pub(super) fn carry_notice(item: &WorkItem) -> String {
    format!(
        "AgentSCM: session {session} leads {id} under recorded agent {agent}, and the lead credential did not match this caller. This call keeps the lead and records the new agent. Generation stays {generation}.",
        session = item.lead_session,
        id = item.id,
        agent = super::advice::agent_name(&item.lead_agent),
        generation = item.generation,
    )
}

/// Moves the in-memory lead onto `token` and `agent`. The session and generation
/// stay. The caller mirrors the same change in `hub_work_participants`.
pub(super) struct Carried {
    pub previous_agent: String,
    pub previous_token: String,
}

pub(super) fn apply_carry(
    item: &mut WorkItem,
    token: &str,
    agent: &str,
    owner: &str,
    login: &str,
) -> Carried {
    let previous_agent = item.lead_agent.clone();
    let previous_token = item.lead_token.clone();
    let session = item.lead_session.clone();
    if previous_token != token {
        item.participants
            .retain(|participant| participant.token != token || participant.session != session);
    }
    if let Some(participant) = item.participants.iter_mut().find(|participant| {
        participant.role == WorkRole::Lead
            && participant.session == session
            && participant.token == previous_token
    }) {
        participant.token = token.to_owned();
        participant.agent = agent.to_owned();
    } else {
        item.participants.push(WorkParticipant {
            owner: owner.to_owned(),
            login: login.to_owned(),
            token: token.to_owned(),
            agent: agent.to_owned(),
            session,
            role: WorkRole::Lead,
        });
    }
    item.lead_token = token.to_owned();
    item.lead_agent = agent.to_owned();
    Carried {
        previous_agent,
        previous_token,
    }
}
