//! Hand-off pickups. A person hands a task, or an existing Work item, to their own coding agent
//! with one line from the Hub. The Hub records the hand-off, and when an `agentscm_start` by one
//! of that person's agent tokens in the same repository carries the record's `request_key` (or
//! resumes its `work_id`), the record is marked picked up with the agent, session and time.
//! A record nobody picks up expires; the person can abandon a waiting one.
use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use super::http::{ApiError, JsonBody, blocking};
use super::intent::text;
use super::login::HumanSession;
use super::store::{unix_now, validate_repository};
use super::{HubError, HubState, Result};

/// How long a hand-off waits for its agent before it expires.
pub const PICKUP_TTL_SECONDS: i64 = 60 * 60;
/// Records older than this are pruned when the same person creates another.
pub const PICKUP_RETAIN_SECONDS: i64 = 7 * 24 * 60 * 60;
/// Retained records per person; a create drops that person's oldest beyond it.
pub const MAX_PICKUPS_PER_OWNER: i64 = 200;
/// How many of a person's newest records the read returns.
const PICKUP_PAGE: usize = 50;
const MAX_TITLE_BYTES: usize = 500;
const MAX_ID_BYTES: usize = 255;

/// The stored columns, in the order [`Pickup::stored`] reads them.
pub(super) const PICKUP_COLUMNS: &str = "id, repository, work_id, title, created_at, expires_at, \
     abandoned_at, picked_at, picked_agent, picked_session, picked_work";

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PickupState {
    Waiting,
    PickedUp,
    Expired,
    Abandoned,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct PickedUp {
    /// The agent token's name.
    pub agent: String,
    pub session: String,
    /// The Work the start claimed, created or joined.
    pub work_id: String,
    pub at: i64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Pickup {
    pub id: String,
    pub repository: String,
    /// The key the line gives `agentscm_start` for a new task; `None` for a Work hand-off,
    /// which is picked up by a start that resumes or joins `work_id`.
    pub request_key: Option<String>,
    pub work_id: Option<String>,
    pub title: Option<String>,
    pub state: PickupState,
    pub created_at: i64,
    pub expires_at: i64,
    pub abandoned_at: Option<i64>,
    pub picked_up: Option<PickedUp>,
}

pub(super) type PickupRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
    Option<String>,
    Option<String>,
);

impl Pickup {
    pub(super) fn stored(row: PickupRow, now: i64) -> Self {
        let (id, repository, work_id, title, created_at, expires_at, abandoned_at, picked_at) =
            (row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7);
        let picked_up = match (picked_at, row.8, row.9, row.10) {
            (Some(at), Some(agent), Some(session), Some(work_id)) => Some(PickedUp {
                agent,
                session,
                work_id,
                at,
            }),
            _ => None,
        };
        let state = if picked_up.is_some() {
            PickupState::PickedUp
        } else if abandoned_at.is_some() {
            PickupState::Abandoned
        } else if expires_at <= now {
            PickupState::Expired
        } else {
            PickupState::Waiting
        };
        Self {
            request_key: work_id.is_none().then(|| id.clone()),
            id,
            repository,
            work_id,
            title,
            state,
            created_at,
            expires_at,
            abandoned_at,
            picked_up,
        }
    }
}

/// Checks a create's caller text before the store writes it.
pub(super) fn validate_new(
    repository: &str,
    work_id: Option<&str>,
    title: Option<&str>,
) -> Result<()> {
    validate_repository(repository)?;
    if let Some(work_id) = work_id {
        super::work::validate_id("work_id", work_id, "wrk")?;
    }
    if let Some(title) = title {
        text("title", title, MAX_TITLE_BYTES)?;
    }
    Ok(())
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/session/pickups", get(list).post(create))
        .route("/v1/hub/session/pickups/abandon", post(abandon))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    repository: String,
    work_id: Option<String>,
    title: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AbandonBody {
    repository: String,
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    repository: String,
}

#[derive(Serialize)]
struct PickupPage {
    pickups: Vec<Pickup>,
}

fn pickup_error(error: HubError) -> ApiError {
    match error {
        HubError::Invalid(message) => ApiError::invalid().with_message(message),
        other => other.into(),
    }
}

async fn create(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<CreateBody>,
) -> std::result::Result<Json<Pickup>, ApiError> {
    let user = human.web_user()?.clone();
    validate_new(
        &body.repository,
        body.work_id.as_deref(),
        body.title.as_deref(),
    )
    .map_err(pickup_error)?;
    // The same allow-list and forge role checks a read-token mint makes.
    let repository =
        super::login::devices::mint_check(&state, &user, &body.repository, "pickup").await?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.pickup_create(
                &user.id,
                &repository.full_name,
                body.work_id.as_deref(),
                body.title.as_deref(),
                unix_now(),
            )
        })
        .await
        .map_err(pickup_error)?,
    ))
}

async fn abandon(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<AbandonBody>,
) -> std::result::Result<Json<Pickup>, ApiError> {
    let owner = human.web_user()?.id.clone();
    validate_repository(&body.repository).map_err(pickup_error)?;
    text("id", &body.id, MAX_ID_BYTES).map_err(pickup_error)?;
    let store = state.store.clone();
    let found = blocking(&state, move || {
        store.pickup_abandon(&owner, &body.repository, &body.id, unix_now())
    })
    .await
    .map_err(pickup_error)?;
    found
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found"))
}

async fn list(
    State(state): State<HubState>,
    human: HumanSession,
    query: std::result::Result<Query<ListQuery>, QueryRejection>,
) -> std::result::Result<Json<PickupPage>, ApiError> {
    let owner = human.web_user()?.id.clone();
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    validate_repository(&query.repository).map_err(pickup_error)?;
    let store = state.store.clone();
    let pickups = blocking(&state, move || {
        store.pickups_of_owner(&owner, &query.repository, unix_now(), PICKUP_PAGE)
    })
    .await
    .map_err(pickup_error)?;
    Ok(Json(PickupPage { pickups }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(
        work_id: Option<&str>,
        abandoned: Option<i64>,
        picked: Option<i64>,
        expires: i64,
    ) -> PickupRow {
        let picked_part = picked.map(|_| ("agent".to_owned(), "s".to_owned(), "wrk_1".to_owned()));
        (
            "pku_1".into(),
            "acme/widgets".into(),
            work_id.map(str::to_owned),
            None,
            10,
            expires,
            abandoned,
            picked,
            picked_part.as_ref().map(|p| p.0.clone()),
            picked_part.as_ref().map(|p| p.1.clone()),
            picked_part.map(|p| p.2),
        )
    }

    #[test]
    fn state_and_request_key_follow_the_stored_row() {
        let waiting = Pickup::stored(row(None, None, None, 100), 50);
        assert_eq!(waiting.state, PickupState::Waiting);
        assert_eq!(waiting.request_key.as_deref(), Some("pku_1"));
        assert_eq!(
            Pickup::stored(row(None, None, None, 100), 100).state,
            PickupState::Expired
        );
        assert_eq!(
            Pickup::stored(row(None, Some(20), None, 100), 50).state,
            PickupState::Abandoned
        );
        let picked = Pickup::stored(row(Some("wrk_1"), None, Some(30), 100), 500);
        assert_eq!(picked.state, PickupState::PickedUp);
        assert_eq!(picked.request_key, None);
        assert_eq!(picked.picked_up.unwrap().work_id, "wrk_1");
    }
}
