//! Who may use the Hub and what each person may do on each repository, when the program that
//! embeds the Hub decides it instead of the allow list and the Git host.

use axum::http::StatusCode;

use super::connection::ConnectedRepository;
use super::http::{ApiError, blocking};
use super::store::{Principal, User};
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
/// `AGENTSCM_ADMINS` may always sign in and reach a repository's data only through a grant,
/// but can still switch any connected repository's conversation sharing, skill status and digest.
pub trait Access: Send + Sync + 'static {
    /// The repositories `person` may use, or `None` when they may not use this Hub at all.
    /// Asked on every request, so a change takes effect on the next one. Before a person's first
    /// sign-in `person.id` is empty: decide by `person.identity`. Only repositories on the
    /// person's own forge host count. Runs on a blocking thread.
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

/// The level `grants` give on the repository a token was minted for: its forge id on the
/// owner's forge host, which a rename keeps. A token with no repository id gets none.
pub(in crate::hub) fn token_level(
    grants: &[Grant],
    host: &str,
    repository_id: Option<i64>,
) -> Option<Level> {
    let id = repository_id?;
    grants
        .iter()
        .find(|grant| grant.repository.repository_id == id && grant.repository.host == host)
        .map(|grant| grant.level)
}

/// The refusal for a person the embedder does not admit, or not to this repository.
pub(in crate::hub) fn not_granted() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "insufficient_role")
        .with_message("You may not use this repository on this Hub")
}

/// Asks the embedder about `person`; one store-bound call.
pub(in crate::hub) async fn decide(state: &HubState, person: &User) -> Result<Decision> {
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

/// An agent or web read token as the embedder allows it now: `None` when it no longer grants
/// the token's repository to its owner; otherwise the token, read-only when they may only read.
/// Every route that takes a token goes through this.
pub(in crate::hub) async fn token(
    state: &HubState,
    mut principal: Principal,
) -> Result<Option<Principal>> {
    let level = match decide(state, &principal.owner).await? {
        Decision::HubRules => return Ok(Some(principal)),
        Decision::Refused => None,
        Decision::Granted(grants) => token_level(
            &grants,
            &principal.owner.identity.host,
            principal.repository_id,
        ),
    };
    Ok(level.map(|level| {
        principal.read_only |= level == Level::Read;
        principal
    }))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::backend::LocalBackend;
    use crate::hub::store::{self, ForgeIdentity};

    /// Grants whatever the test sets, to anyone.
    struct Fixed(Mutex<Option<Vec<Grant>>>);

    impl Access for Fixed {
        fn grants(&self, _: &User) -> Result<Option<Vec<Grant>>> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    fn grant(host: &str, repository_id: i64, full_name: &str, level: Level) -> Grant {
        Grant {
            repository: ConnectedRepository {
                id: format!("rep_{repository_id}"),
                connection: "con_test".into(),
                host: host.into(),
                repository_id,
                full_name: full_name.into(),
                node_id: None,
                installation_id: None,
                pulls_etag: None,
            },
            level,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_token_follows_its_owners_grant_on_its_repository() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let objects = Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap());
        let principal = |repository_id| Principal {
            token_id: "tok_test".into(),
            name: "agent".into(),
            repository: "acme/widgets".into(),
            repository_id,
            expires_at: i64::MAX,
            read_only: false,
            owner: User {
                id: "usr_test".into(),
                identity: ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 7,
                },
                login: "maya".into(),
                name: None,
                disabled: false,
            },
        };
        let plain = HubState::new(store.clone(), objects.clone());
        let unchanged = token(&plain, principal(Some(101))).await.unwrap();
        assert_eq!(unchanged.map(|p| p.read_only), Some(false), "no embedder");

        let fixed = Arc::new(Fixed(Mutex::new(None)));
        let state = HubState::new(store, objects).with_access(fixed.clone());
        let read_only = |grants: Vec<Grant>, repository_id| {
            *fixed.0.lock().unwrap() = Some(grants);
            let (state, principal) = (state.clone(), principal(repository_id));
            async move { token(&state, principal).await.unwrap().map(|p| p.read_only) }
        };
        let github = |level| grant("github.com", 101, "acme/widgets", level);
        assert_eq!(
            read_only(vec![github(Level::Contribute)], Some(101)).await,
            Some(false)
        );
        assert_eq!(
            read_only(vec![github(Level::Read)], Some(101)).await,
            Some(true)
        );
        // A rename keeps the grant: tokens are matched by forge id on the owner's host.
        let renamed = grant("github.com", 101, "acme/gadgets", Level::Contribute);
        assert_eq!(read_only(vec![renamed], Some(101)).await, Some(false));
        let elsewhere = grant("gitlab.example", 101, "acme/widgets", Level::Contribute);
        assert_eq!(read_only(vec![elsewhere], Some(101)).await, None);
        assert_eq!(read_only(vec![github(Level::Administer)], None).await, None);
        assert_eq!(read_only(Vec::new(), Some(101)).await, None);
        *fixed.0.lock().unwrap() = None;
        assert!(token(&state, principal(Some(101))).await.unwrap().is_none());
    }
}
