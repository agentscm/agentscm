//! A person's own agent setup problems (#292): blockers their agents reported through setup
//! verification, an edit hook still unverified while setup runs, and agent tokens for the
//! repository that no longer work. Reported problems come from the Hub's short-lived onboarding
//! observations, which a restart clears; token problems come from stored agent tokens. None of
//! this is an authorization decision or a host attestation, and no credential is read.

use super::Result;
use super::onboarding::{Blocker, Observations};
use super::store::{Principal, Store};

/// A repository's newest agent token counts for this long after its expiry time, so a computer
/// that stopped renewing (or was revoked) shows up without every long-idle repository raising a
/// badge.
pub const RECENT_EXPIRY_SECONDS: i64 = 24 * 60 * 60;

/// Why an agent cannot coordinate. `code` is stable for other surfaces to match on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupReason {
    /// The coding agent itself is not signed in.
    Login,
    /// The host has not approved the AgentSCM hooks or MCP server.
    Trust,
    /// The coding agent reported a usage limit.
    Quota,
    /// The provider rejected this client and account.
    Unsupported,
    /// The agent reported a setup failure it could not classify.
    Unknown,
    /// MCP reached the Hub in a verifying session, but no native edit hook has.
    EditHookUnverified,
    /// No agent token for the repository is valid; the newest one expired.
    TokenExpired,
    /// No agent token for the repository is valid; the newest one was revoked.
    TokenRevoked,
}

impl SetupReason {
    pub fn code(self) -> &'static str {
        match self {
            Self::Login => "agent_login_required",
            Self::Trust => "agent_trust_required",
            Self::Quota => "agent_quota",
            Self::Unsupported => "agent_unsupported",
            Self::Unknown => "agent_setup_unknown",
            Self::EditHookUnverified => "edit_hook_unverified",
            Self::TokenExpired => "agent_token_expired",
            Self::TokenRevoked => "agent_token_revoked",
        }
    }

    /// The end of "<login>'s <agent> …".
    pub fn what(self) -> &'static str {
        match self {
            Self::Login => "needs you to sign in to it",
            Self::Trust => "needs its AgentSCM hooks and MCP server approved",
            Self::Quota => "has hit a usage limit",
            Self::Unsupported => "can't verify with this account",
            Self::Unknown => "reported a setup problem",
            Self::EditHookUnverified => "reached the Hub over MCP, but no edit hook has",
            Self::TokenExpired => "has no valid token for this repository: it expired",
            Self::TokenRevoked => "has no valid token for this repository: it was revoked",
        }
    }

    /// What the person does about it.
    pub fn next_action(self) -> &'static str {
        match self {
            Self::Login => "Sign in using the coding agent's own login flow, then resume setup.",
            Self::Trust => {
                "Review and approve the AgentSCM plugin, hooks and MCP server in the host yourself, then start a fresh session."
            }
            Self::Quota => {
                "Wait for the agent's usage reset or use an existing authorized plan, then verify setup again."
            }
            Self::Unsupported => {
                "Check supported access with the provider; do not switch credentials or billing silently."
            }
            Self::Unknown => {
                "Inspect the host's login, MCP and hook status, then verify setup again."
            }
            Self::EditHookUnverified => {
                "Ask the agent to use a native edit tool on a disposable file in the same session, then call agentscm_setup_status again."
            }
            Self::TokenExpired | Self::TokenRevoked => {
                "Sign this computer in to the Hub again (`agentscm hub login`, or Sign In in the menu bar app) so the agent gets a new token."
            }
        }
    }

    fn of(blocker: Blocker) -> Self {
        match blocker {
            Blocker::Login => Self::Login,
            Blocker::Trust => Self::Trust,
            Blocker::Quota => Self::Quota,
            Blocker::Unsupported => Self::Unsupported,
            Blocker::Unknown => Self::Unknown,
        }
    }
}

/// One problem of the viewer's own agents in the viewer's repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupProblem {
    pub reason: SetupReason,
    /// Stable per agent connection and session (or per repository for a token problem); no secret.
    pub key: String,
    /// The display name of the agent token concerned.
    pub agent: String,
    /// Agent token ids concerned, to link the Work they lead. Never shown.
    pub tokens: Vec<String>,
}

/// The viewer's own problems in the repository `who` reads. Onboarding rows of a revoked
/// token or a disabled person are dropped, as the Setup page drops them.
pub fn problems(
    store: &dyn Store,
    observations: &Observations,
    who: &Principal,
    now: i64,
) -> Result<Vec<SetupProblem>> {
    let owner = &who.owner;
    if store
        .user_by_id(&owner.id)?
        .is_none_or(|user| user.disabled)
    {
        return Ok(Vec::new());
    }
    let tokens = store
        .list_agent_tokens(&owner.id)?
        .into_iter()
        // A token minted before the forge confirmed an id (`mint_agent_token`) still counts.
        .filter(|t| {
            !t.read_only
                && t.repository == who.repository
                && (t.repository_id.is_none()
                    || who.repository_id.is_none()
                    || t.repository_id == who.repository_id)
        })
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    for (token, status) in observations.owned_rows(who, &owner.id, now)? {
        let Some(name) = tokens
            .iter()
            .find(|t| t.id == token && !t.revoked && t.expires_at > now)
            .map(|t| t.name.clone())
        else {
            continue;
        };
        let reason = match status.blocker {
            Some(blocker) => SetupReason::of(blocker),
            None if status.mcp_verified && !status.native_verified => {
                SetupReason::EditHookUnverified
            }
            None => continue,
        };
        out.push(SetupProblem {
            reason,
            key: status.key,
            agent: name,
            tokens: vec![token],
        });
    }
    if !tokens.iter().any(|t| !t.revoked && t.expires_at > now)
        && let Some(newest) = tokens.iter().max_by_key(|t| (t.created_at, t.id.clone()))
        && newest.expires_at > now - RECENT_EXPIRY_SECONDS
    {
        out.push(SetupProblem {
            reason: if newest.revoked {
                SetupReason::TokenRevoked
            } else {
                SetupReason::TokenExpired
            },
            key: "repository".into(),
            agent: newest.name.clone(),
            tokens: tokens.iter().map(|t| t.id.clone()).collect(),
        });
    }
    Ok(out)
}
