//! The digest's read route and the two switches that decide who is sent it. Delivery is off for
//! a repository until a repository admin (GitHub `admin`, GitLab maintainer or owner) or a Hub
//! admin switches it on, and each person can switch their own off. Reading the digest needs
//! neither switch. Values live in `hub_settings`, keyed by the forge's repository id so a rename
//! keeps them.
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Query, State};
use axum::routing::{get, put};
use serde::{Deserialize, Serialize};

use super::digest::{self, DigestQuery, WorkDigestV1};
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::store::{Principal, Store, unix_now};
use super::{HubError, HubState, Result};

fn repository_key(repository_id: i64) -> String {
    format!("work_digest:repo:{repository_id}")
}

fn opt_out_key(user_id: &str, repository_id: i64) -> String {
    format!("work_digest:opt_out:{user_id}:{repository_id}")
}

/// The digest switches as one person sees them. Readers ignore fields they do not know.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DigestSettings {
    pub repository: String,
    /// The repository's switch.
    pub enabled: bool,
    /// This person's own switch.
    pub opted_out: bool,
    /// `enabled` and not `opted_out`: this person is sent the digest where they have a
    /// delivery binding.
    pub delivering: bool,
}

/// The switches for `user_id` in a repository. A token without a forge repository id (minted
/// before ids were kept) is never sent a digest, and an unreadable value reads as off.
pub fn settings(
    store: &dyn Store,
    repository: &str,
    repository_id: Option<i64>,
    user_id: &str,
) -> Result<DigestSettings> {
    let (enabled, opted_out) = match repository_id {
        Some(id) => (
            store.get_setting(&repository_key(id))?.as_deref() == Some("on"),
            !matches!(
                store.get_setting(&opt_out_key(user_id, id))?.as_deref(),
                None | Some("0")
            ),
        ),
        None => (false, false),
    };
    Ok(DigestSettings {
        repository: repository.to_owned(),
        enabled,
        opted_out,
        delivering: enabled && !opted_out,
    })
}

/// Switches the repository's digest on or off; the caller has checked that `actor` may.
pub fn set_repository(
    store: &dyn Store,
    repository_id: i64,
    enabled: bool,
    actor: &str,
) -> Result<()> {
    let value = if enabled { "on" } else { "off" };
    store.set_setting(&repository_key(repository_id), value, actor)
}

/// Records `user_id`'s own choice for one repository.
pub fn set_opted_out(
    store: &dyn Store,
    user_id: &str,
    repository_id: i64,
    opted_out: bool,
) -> Result<()> {
    let value = if opted_out { "1" } else { "0" };
    store.set_setting(&opt_out_key(user_id, repository_id), value, user_id)
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/standup/digest", get(read))
        .route(
            "/v1/hub/standup/digest/settings",
            get(view).put(set_enabled),
        )
        .route("/v1/hub/standup/digest/settings/me", put(set_mine))
        .route(
            "/v1/hub/standup/merged-release/settings",
            get(view_release).put(set_release),
        )
}

async fn read(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<DigestQuery>, QueryRejection>,
) -> std::result::Result<Json<WorkDigestV1>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            digest::read(&*store, &principal, &query, unix_now())
        })
        .await?,
    ))
}

fn view_of(store: &dyn Store, principal: &Principal) -> Result<DigestSettings> {
    settings(
        store,
        &principal.repository,
        principal.repository_id,
        &principal.owner.id,
    )
}

async fn view(
    State(state): State<HubState>,
    Reader(principal): Reader,
) -> std::result::Result<Json<DigestSettings>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || view_of(&*store, &principal)).await?,
    ))
}

fn no_repository_id() -> HubError {
    HubError::Invalid(
        "this token has no forge repository id; connect the checkout again to mint one that has"
            .into(),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetEnabled {
    enabled: bool,
}

async fn set_enabled(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(body): JsonBody<SetEnabled>,
) -> std::result::Result<Json<DigestSettings>, ApiError> {
    let repository_id = principal.repository_id.ok_or_else(no_repository_id)?;
    // A Hub admin changes any connected repository's switch, with or without a forge role;
    // anyone else needs the forge to say now that they administer the repository.
    let admin = state.login.is_admin_user(&principal.owner)
        || super::login::devices::mint_access(
            &state,
            &principal.owner,
            &principal.repository,
            "work_digest",
        )
        .await?
        .1;
    if !admin {
        return Err(ApiError::forbidden()
            .with_message("Only a repository admin or a Hub admin switches the digest on or off"));
    }
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            set_repository(&*store, repository_id, body.enabled, &principal.owner.id)?;
            view_of(&*store, &principal)
        })
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetMine {
    opted_out: bool,
}

async fn set_mine(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(body): JsonBody<SetMine>,
) -> std::result::Result<Json<DigestSettings>, ApiError> {
    let repository_id = principal.repository_id.ok_or_else(no_repository_id)?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            set_opted_out(&*store, &principal.owner.id, repository_id, body.opted_out)?;
            view_of(&*store, &principal)
        })
        .await?,
    ))
}

/// Whether a repository releases the hold of Work its merged pull request finished.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct MergedReleaseSettings {
    pub repository: String,
    pub enabled: bool,
}

fn release_view_of(store: &dyn Store, principal: &Principal) -> MergedReleaseSettings {
    MergedReleaseSettings {
        repository: principal.repository.clone(),
        enabled: principal
            .repository_id
            .is_some_and(|id| super::merged_holds::merged_release_enabled(store, id)),
    }
}

async fn view_release(
    State(state): State<HubState>,
    Reader(principal): Reader,
) -> std::result::Result<Json<MergedReleaseSettings>, ApiError> {
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || Ok(release_view_of(&*store, &principal))).await?,
    ))
}

async fn set_release(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(body): JsonBody<SetEnabled>,
) -> std::result::Result<Json<MergedReleaseSettings>, ApiError> {
    let repository_id = principal.repository_id.ok_or_else(no_repository_id)?;
    // The same standing as the digest switch: a Hub admin or a repository admin.
    let admin = state.login.is_admin_user(&principal.owner)
        || super::login::devices::mint_access(
            &state,
            &principal.owner,
            &principal.repository,
            "merged_release",
        )
        .await?
        .1;
    if !admin {
        return Err(ApiError::forbidden().with_message(
            "Only a repository admin or a Hub admin switches automatic release of merged Work on or off",
        ));
    }
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            super::merged_holds::set_merged_release(
                &*store,
                repository_id,
                body.enabled,
                &principal.owner.id,
            )?;
            Ok(release_view_of(&*store, &principal))
        })
        .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_is_off_until_the_repository_is_on_and_each_person_can_leave() {
        let dir = tempfile::tempdir().unwrap();
        let url = format!("sqlite://{}", dir.path().join("hub.db").display());
        let store = crate::hub::store::open(&url).unwrap();
        let store = &*store;
        let of = |user: &str, id| settings(store, "o/r", id, user).unwrap();
        assert!(!of("usr_a", Some(7)).delivering, "off by default");
        set_opted_out(store, "usr_b", 7, true).unwrap();
        set_repository(store, 7, true, "usr_admin").unwrap();
        assert!(of("usr_a", Some(7)).delivering);
        assert!(!of("usr_a", Some(8)).enabled, "per repository");
        assert!(!of("usr_a", None).enabled, "no forge id is never sent one");
        let left = of("usr_b", Some(7));
        assert!(
            left.enabled && left.opted_out && !left.delivering,
            "per person"
        );
        set_opted_out(store, "usr_b", 7, false).unwrap();
        assert!(of("usr_b", Some(7)).delivering);
        store
            .set_setting(&opt_out_key("usr_b", 7), "unreadable", "usr_b")
            .unwrap();
        assert!(
            !of("usr_b", Some(7)).delivering,
            "unreadable is out, not in"
        );
        set_repository(store, 7, false, "usr_admin").unwrap();
        assert!(!of("usr_a", Some(7)).delivering);
    }
}
