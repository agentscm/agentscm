//! Who may use the Hub and what each person may do on each repository, when the program that
//! embeds the Hub decides it instead of the allow list and the Git host.

use axum::http::StatusCode;

use super::connection::ConnectedRepository;
use super::http::{ApiError, blocking};
use super::store::User;
use super::{HubState, Result};

/// What a person may do on one repository, least first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// See its tasks, sessions and conversations, and change nothing.
    Read,
    /// Also run agents, hold and hand over tasks, and answer for agents.
    Contribute,
    /// Also change the repository's own settings.
    Administer,
}

/// One repository a person may use, and how.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    pub repository: ConnectedRepository,
    pub level: Level,
}

/// Decides who may use the Hub and what each person may do, in place of the allow list and
/// the Git host's role on each repository. A program that embeds the Hub sets one with
/// [`HubState::with_access`]; without one the Hub keeps its own rules. Hub admins named in
/// `AGENTSCM_ADMINS` may always sign in, but reach a repository only through a grant.
pub trait Access: Send + Sync + 'static {
    /// The repositories `person` may use, or `None` when they may not use this Hub at all.
    /// Asked on every request, so a change takes effect on the next one. Before a person's first
    /// sign-in `person.id` is empty: decide by `person.identity`. Runs on a blocking thread.
    fn grants(&self, person: &User) -> Result<Option<Vec<Grant>>>;
}

/// The embedder's answer for one person, or that there is no embedder.
pub(in crate::hub) enum Decision {
    /// No embedder: the allow list and the Git host decide.
    HubRules,
    Refused,
    Granted(Vec<Grant>),
}

impl Decision {
    /// The level granted on the repository with Hub id `repository`, if any.
    pub(in crate::hub) fn level(&self, repository: &str) -> Option<Level> {
        match self {
            Self::Granted(grants) => grants
                .iter()
                .find(|grant| grant.repository.id == repository)
                .map(|grant| grant.level),
            Self::HubRules | Self::Refused => None,
        }
    }
}

/// The level `grants` give on the repository a token was minted for, named by its full name and
/// forge id.
pub(in crate::hub) fn token_level(
    grants: &[Grant],
    repository: &str,
    repository_id: Option<i64>,
) -> Option<Level> {
    grants
        .iter()
        .find(|grant| {
            Some(grant.repository.repository_id) == repository_id
                && grant.repository.full_name.eq_ignore_ascii_case(repository)
        })
        .map(|grant| grant.level)
}

/// The refusal for a person the embedder does not admit, or not to this repository.
pub(in crate::hub) fn not_granted() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "insufficient_role")
        .with_message("You may not use this repository on this Hub")
}

/// Asks the embedder about `person`; one store-bound call.
pub(in crate::hub) async fn decide(
    state: &HubState,
    person: &User,
) -> std::result::Result<Decision, ApiError> {
    let Some(access) = state.access.clone() else {
        return Ok(Decision::HubRules);
    };
    let person = person.clone();
    Ok(
        match blocking(state, move || access.grants(&person)).await? {
            None => Decision::Refused,
            Some(grants) => Decision::Granted(grants),
        },
    )
}
