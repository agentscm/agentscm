//! Whether agent conversations in a repository are shared with the Hub. A repository setting,
//! changed by a repository admin (GitHub `admin`, GitLab maintainer or owner) or a Hub admin,
//! falls back to a Hub-wide default that only Hub admins change; both start off. Each person can
//! opt their own sessions out, and is told once in their agent's session context that sharing is
//! on. Connected checkouts follow the answer: the client reads it at session start and on hook
//! calls, and the Hub refuses uploads while it says no. Values live in `hub_settings`, keyed by
//! the forge's repository id so a rename keeps them.
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Path, Query, State};
use axum::routing::{get, put};
use serde::{Deserialize, Serialize};

use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::login::HumanSession;
use super::store::{Store, validate_repository};
use super::{HubState, Result};

const HUB_DEFAULT_KEY: &str = "conversation_sharing:default";

fn repository_key(repository_id: i64) -> String {
    format!("conversation_sharing:repo:{repository_id}")
}

/// A person's own choice for one repository, each under its own key so the session-start
/// notice and an opt-out never overwrite each other.
fn person_key(choice: &str, user_id: &str, repository_id: i64) -> String {
    format!("conversation_sharing:{choice}:{user_id}:{repository_id}")
}

/// A person's own choices for one repository.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Person {
    opted_out: bool,
    /// Their agent has shown the one-time notice that sharing is on.
    notified: bool,
}

/// What a person's checkouts of a repository do, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Sharing {
    pub repository: String,
    /// The repository's effective setting.
    pub enabled: bool,
    /// `repository` when the repository sets it, else `hub_default`.
    pub source: &'static str,
    pub hub_default: bool,
    pub opted_out: bool,
    pub notified: bool,
    /// `enabled` and not `opted_out`: this person's sessions upload conversation bodies.
    pub sharing: bool,
    /// Web sign-in only: whether this person may change the repository setting.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_change: Option<bool>,
    /// Web sign-in only: whether this person may change the Hub-wide default (Hub admins).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub can_change_hub_default: Option<bool>,
}

fn flag(store: &dyn Store, key: &str) -> Result<Option<bool>> {
    Ok(match store.get_setting(key)?.as_deref() {
        Some("on") => Some(true),
        Some("off") => Some(false),
        _ => None,
    })
}

/// An unreadable value opts the person out rather than in.
fn person(store: &dyn Store, user_id: &str, repository_id: i64) -> Result<Person> {
    let choice = |name| -> Result<bool> {
        Ok(
            match store
                .get_setting(&person_key(name, user_id, repository_id))?
                .as_deref()
            {
                None | Some("0") => false,
                Some(_) => true,
            },
        )
    };
    Ok(Person {
        opted_out: choice("opt_out")?,
        notified: choice("notified")?,
    })
}

/// The repository's effective setting. A token without a forge repository id (minted before ids
/// were kept) never shares.
pub(super) fn repository_shares(store: &dyn Store, repository_id: Option<i64>) -> Result<bool> {
    let Some(id) = repository_id else {
        return Ok(false);
    };
    Ok(match flag(store, &repository_key(id))? {
        Some(enabled) => enabled,
        None => flag(store, HUB_DEFAULT_KEY)?.unwrap_or(false),
    })
}

/// Whether `user_id`'s sessions in the repository may upload conversation bodies now.
pub(super) fn person_shares(
    store: &dyn Store,
    user_id: &str,
    repository_id: Option<i64>,
) -> Result<bool> {
    let Some(id) = repository_id else {
        return Ok(false);
    };
    Ok(repository_shares(store, repository_id)? && !person(store, user_id, id)?.opted_out)
}

fn view(
    store: &dyn Store,
    repository: &str,
    repository_id: Option<i64>,
    user_id: &str,
) -> Result<Sharing> {
    let hub_default = flag(store, HUB_DEFAULT_KEY)?.unwrap_or(false);
    let own = match repository_id {
        Some(id) => flag(store, &repository_key(id))?,
        None => None,
    };
    let enabled = repository_id.is_some() && own.unwrap_or(hub_default);
    let mine = match repository_id {
        Some(id) => person(store, user_id, id)?,
        None => Person::default(),
    };
    Ok(Sharing {
        repository: repository.to_owned(),
        enabled,
        source: if own.is_some() {
            "repository"
        } else {
            "hub_default"
        },
        hub_default,
        opted_out: mine.opted_out,
        notified: mine.notified,
        sharing: enabled && !mine.opted_out,
        can_change: None,
        can_change_hub_default: None,
    })
}

fn update_person(
    store: &dyn Store,
    user_id: &str,
    repository_id: i64,
    change: &PersonChange,
) -> Result<()> {
    for (name, value) in [("opt_out", change.opted_out), ("notified", change.notified)] {
        if let Some(value) = value {
            let key = person_key(name, user_id, repository_id);
            store.set_setting(&key, if value { "1" } else { "0" }, user_id)?;
        }
    }
    Ok(())
}

fn on_off(enabled: Option<bool>) -> &'static str {
    match enabled {
        Some(true) => "on",
        Some(false) => "off",
        None => "default",
    }
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/repos/{repo}/conversation-sharing", get(token_view))
        .route(
            "/v1/hub/repos/{repo}/conversation-sharing/me",
            put(token_person),
        )
        .route(
            "/v1/hub/session/conversation-sharing",
            get(session_view).put(session_set),
        )
        .route(
            "/v1/hub/session/conversation-sharing/me",
            put(session_person),
        )
        .route(
            "/v1/hub/session/conversation-sharing/default",
            put(session_default),
        )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonChange {
    #[serde(default)]
    opted_out: Option<bool>,
    #[serde(default)]
    notified: Option<bool>,
}

async fn token_view(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(repo): Path<String>,
) -> std::result::Result<Json<Sharing>, ApiError> {
    if principal.repository != repo {
        return Err(ApiError::unauthorized());
    }
    let store = state.store.clone();
    let sharing = blocking(&state, move || {
        view(
            &*store,
            &principal.repository,
            principal.repository_id,
            &principal.owner.id,
        )
    })
    .await?;
    Ok(Json(sharing))
}

async fn token_person(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(repo): Path<String>,
    JsonBody(change): JsonBody<PersonChange>,
) -> std::result::Result<Json<Sharing>, ApiError> {
    if principal.repository != repo {
        return Err(ApiError::unauthorized());
    }
    let Some(repository_id) = principal.repository_id else {
        return Err(ApiError::invalid());
    };
    let store = state.store.clone();
    let sharing = blocking(&state, move || {
        update_person(&*store, &principal.owner.id, repository_id, &change)?;
        view(
            &*store,
            &principal.repository,
            principal.repository_id,
            &principal.owner.id,
        )
    })
    .await?;
    Ok(Json(sharing))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryQuery {
    repository: String,
}

/// The signed-in person, a repository they can read now, and who may change what:
/// `(user id, full name, repository id, repository or Hub admin, Hub admin)`. `via` names the
/// setting the read is for.
pub(super) async fn session_scope(
    state: &HubState,
    human: &HumanSession,
    repository: &str,
    via: &'static str,
) -> std::result::Result<(String, String, i64, bool, bool), ApiError> {
    let user = human.web_user()?.clone();
    validate_repository(repository)?;
    let hub_admin = state.login.is_admin_user(&user);
    // A Hub admin changes any connected repository's setting, with or without a forge role.
    if hub_admin {
        let (store, host, named) = (
            state.store.clone(),
            user.identity.host.clone(),
            repository.to_owned(),
        );
        if let Some(repo) = blocking(state, move || store.repository_named(&host, &named)).await? {
            return Ok((user.id, repo.full_name, repo.repository_id, true, true));
        }
    }
    let (repo, repository_admin) =
        super::login::devices::mint_access(state, &user, repository, via).await?;
    Ok((
        user.id,
        repo.full_name,
        repo.repository_id,
        repository_admin || hub_admin,
        hub_admin,
    ))
}

async fn session_view(
    State(state): State<HubState>,
    human: HumanSession,
    query: std::result::Result<Query<RepositoryQuery>, QueryRejection>,
) -> std::result::Result<Json<Sharing>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let (user, name, id, can_change, hub_admin) =
        session_scope(&state, &human, &query.repository, "conversation_sharing").await?;
    let store = state.store.clone();
    let mut sharing = blocking(&state, move || view(&*store, &name, Some(id), &user)).await?;
    sharing.can_change = Some(can_change);
    sharing.can_change_hub_default = Some(hub_admin);
    Ok(Json(sharing))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetRepository {
    repository: String,
    /// `null` follows the Hub-wide default again.
    enabled: Option<bool>,
}

async fn session_set(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<SetRepository>,
) -> std::result::Result<Json<Sharing>, ApiError> {
    let (user, name, id, can_change, hub_admin) =
        session_scope(&state, &human, &body.repository, "conversation_sharing").await?;
    if !can_change {
        return Err(ApiError::forbidden()
            .with_message("Only a repository admin or a Hub admin changes conversation sharing"));
    }
    let store = state.store.clone();
    let mut sharing = blocking(&state, move || {
        store.set_setting(&repository_key(id), on_off(body.enabled), &user)?;
        view(&*store, &name, Some(id), &user)
    })
    .await?;
    sharing.can_change = Some(true);
    sharing.can_change_hub_default = Some(hub_admin);
    Ok(Json(sharing))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetPerson {
    repository: String,
    opted_out: bool,
}

async fn session_person(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<SetPerson>,
) -> std::result::Result<Json<Sharing>, ApiError> {
    let (user, name, id, can_change, hub_admin) =
        session_scope(&state, &human, &body.repository, "conversation_sharing").await?;
    let store = state.store.clone();
    let change = PersonChange {
        opted_out: Some(body.opted_out),
        notified: None,
    };
    let mut sharing = blocking(&state, move || {
        update_person(&*store, &user, id, &change)?;
        view(&*store, &name, Some(id), &user)
    })
    .await?;
    sharing.can_change = Some(can_change);
    sharing.can_change_hub_default = Some(hub_admin);
    Ok(Json(sharing))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetDefault {
    enabled: bool,
}

#[derive(Serialize)]
struct HubDefault {
    hub_default: bool,
}

async fn session_default(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<SetDefault>,
) -> std::result::Result<Json<HubDefault>, ApiError> {
    let user = human.web_user()?.clone();
    if !state.login.is_admin_user(&user) {
        return Err(ApiError::forbidden()
            .with_message("Only a Hub admin changes the Hub-wide conversation sharing default"));
    }
    let store = state.store.clone();
    blocking(&state, move || {
        store.set_setting(HUB_DEFAULT_KEY, on_off(Some(body.enabled)), &user.id)
    })
    .await?;
    Ok(Json(HubDefault {
        hub_default: body.enabled,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_setting_overrides_the_hub_default_and_people_opt_out() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.db").display());
        let store = crate::hub::store::open(&url).unwrap();
        let store = &*store;
        assert!(
            !repository_shares(store, Some(7)).unwrap(),
            "off by default"
        );
        store.set_setting(HUB_DEFAULT_KEY, "on", "admin").unwrap();
        assert!(repository_shares(store, Some(7)).unwrap());
        assert!(
            !repository_shares(store, None).unwrap(),
            "no forge id never shares"
        );
        store
            .set_setting(&repository_key(7), "off", "admin")
            .unwrap();
        assert!(!repository_shares(store, Some(7)).unwrap());
        assert_eq!(
            view(store, "o/r", Some(7), "usr_a").unwrap().source,
            "repository"
        );
        store
            .set_setting(&repository_key(7), "default", "admin")
            .unwrap();
        assert!(person_shares(store, "usr_a", Some(7)).unwrap());
        let out = PersonChange {
            opted_out: Some(true),
            notified: None,
        };
        update_person(store, "usr_a", 7, &out).unwrap();
        assert!(!person_shares(store, "usr_a", Some(7)).unwrap());
        assert!(
            person_shares(store, "usr_b", Some(7)).unwrap(),
            "per person"
        );
        let told = PersonChange {
            opted_out: None,
            notified: Some(true),
        };
        update_person(store, "usr_a", 7, &told).unwrap();
        let mine = view(store, "o/r", Some(7), "usr_a").unwrap();
        assert!(mine.enabled && mine.opted_out && mine.notified && !mine.sharing);
    }
}
