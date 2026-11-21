//! Authenticated owner-addressed notice operations.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::notices::{
    NewOwnerNotice, OwnerNotice, OwnerNoticePage, OwnerNoticeRecipient, OwnerNoticeRecipientStatus,
    OwnerNoticeRoute, OwnerNoticeSourceKind,
};
use super::store::{ForgeIdentity, Principal, User, unix_now};
use super::work_transition::WorkLifecycleReader;
use super::{HubError, HubState};

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/owner-notices", post(send).get(list))
        .route("/v1/hub/owner-notices/{id}", get(status))
        .route("/v1/hub/owner-notices/{id}/acknowledge", post(acknowledge))
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum AddressedNoticeTarget {
    Owner(String),
    PullRequest(i64),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressedNoticeSend {
    pub idempotency_key: String,
    pub source_id: String,
    pub target: AddressedNoticeTarget,
    pub body: String,
    #[serde(default)]
    pub route: OwnerNoticeRoute,
}

#[derive(Clone, Debug, Serialize)]
pub struct OwnerNoticeView {
    pub id: String,
    pub source_id: String,
    pub sender_owner: String,
    pub recipient_owner: String,
    pub body: String,
    pub route: OwnerNoticeRoute,
    pub created_at: i64,
    pub queued: bool,
    pub delivered: bool,
    pub delivered_at: Option<i64>,
    pub acknowledged: bool,
    pub acknowledged_at: Option<i64>,
}

impl From<OwnerNotice> for OwnerNoticeView {
    fn from(notice: OwnerNotice) -> Self {
        let delivered = notice.delivery_proven();
        Self {
            id: notice.id,
            source_id: notice.source_id,
            sender_owner: notice.sender_owner,
            recipient_owner: notice.recipient.owner,
            body: notice.body,
            route: notice.route,
            created_at: notice.created_at,
            queued: true,
            delivered,
            delivered_at: notice.delivered_at,
            acknowledged: notice.acknowledged_at.is_some(),
            acknowledged_at: notice.acknowledged_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    after: Option<String>,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    20
}

#[derive(Serialize)]
pub struct OwnerNoticeViews {
    notices: Vec<OwnerNoticeView>,
    next_after: Option<String>,
}

fn recipient(user: User) -> Result<OwnerNoticeRecipient, HubError> {
    if user.disabled {
        return Err(HubError::Unauthorized);
    }
    Ok(OwnerNoticeRecipient {
        owner: user.id,
        provider: user.identity.provider,
        host: user.identity.host,
    })
}

async fn resolve_target(
    state: &HubState,
    principal: &Principal,
    target: AddressedNoticeTarget,
) -> Result<OwnerNoticeRecipient, HubError> {
    if principal.read_only || principal.repository_id.is_none() {
        return Err(HubError::Unauthorized);
    }
    let user = match target {
        AddressedNoticeTarget::Owner(owner) => {
            let store = state.store.clone();
            blocking(state, move || store.user_by_id(&owner)).await?
        }
        AddressedNoticeTarget::PullRequest(number) => {
            if number <= 0 {
                return Err(HubError::Invalid(
                    "pull request number must be positive".into(),
                ));
            }
            let pulls = super::sync::open_pulls(
                state,
                &principal.owner.identity.host,
                &principal.repository,
                principal.repository_id,
            )
            .await?;
            let author = pulls
                .iter()
                .find(|pull| pull["number"].as_i64() == Some(number))
                .and_then(|pull| pull["author"]["id"].as_i64())
                .ok_or(HubError::Unauthorized)?;
            let identity = ForgeIdentity {
                provider: principal.owner.identity.provider.clone(),
                host: principal.owner.identity.host.clone(),
                user_id: author,
            };
            let store = state.store.clone();
            blocking(state, move || store.user_for_identity(&identity)).await?
        }
    }
    .ok_or(HubError::Unauthorized)?;
    if user.identity.provider != principal.owner.identity.provider
        || user.identity.host != principal.owner.identity.host
    {
        return Err(HubError::Unauthorized);
    }
    recipient(user)
}

pub async fn send_for_principal(
    state: &HubState,
    principal: &Principal,
    request: AddressedNoticeSend,
) -> Result<OwnerNoticeView, HubError> {
    let recipient = resolve_target(state, principal, request.target).await?;
    let notice = NewOwnerNotice {
        idempotency_key: request.idempotency_key,
        source_kind: OwnerNoticeSourceKind::Addressed,
        source_id: request.source_id,
        recipient,
        route: request.route,
        body: request.body,
    };
    let store = state.store.clone();
    let token = principal.token_id.clone();
    Ok(blocking(state, move || {
        store.owner_notice_put(&token, &notice, unix_now())
    })
    .await?
    .into())
}

async fn send(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<AddressedNoticeSend>,
) -> Result<Json<OwnerNoticeView>, ApiError> {
    Ok(Json(send_for_principal(&state, &principal, request).await?))
}

pub async fn status_for_principal(
    state: &HubState,
    principal: &Principal,
    id: String,
) -> Result<OwnerNoticeView, HubError> {
    let store = state.store.clone();
    let token = principal.token_id.clone();
    Ok(blocking(state, move || {
        store.owner_notice_get(&token, &id, unix_now())
    })
    .await?
    .into())
}

async fn status(
    State(state): State<HubState>,
    Reader(principal): Reader,
    Path(id): Path<String>,
) -> Result<Json<OwnerNoticeView>, ApiError> {
    Ok(Json(status_for_principal(&state, &principal, id).await?))
}

async fn list(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: Result<Query<ListQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<OwnerNoticeViews>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    Ok(Json(
        list_for_principal(&state, &principal, query.after, query.limit).await?,
    ))
}

pub async fn list_for_principal(
    state: &HubState,
    principal: &Principal,
    after: Option<String>,
    limit: usize,
) -> Result<OwnerNoticeViews, HubError> {
    let store = state.store.clone();
    let owner = principal.owner.clone();
    let repository = principal.repository.clone();
    let repository_id = principal.repository_id.ok_or(HubError::Unauthorized)?;
    let page: OwnerNoticePage = blocking(state, move || {
        store.owner_notice_list(
            &WorkLifecycleReader::Person {
                owner: &owner,
                repository: &repository,
                repository_id,
            },
            after.as_deref(),
            limit,
            unix_now(),
        )
    })
    .await?;
    Ok(OwnerNoticeViews {
        notices: page.notices.into_iter().map(Into::into).collect(),
        next_after: page.next_after,
    })
}

pub async fn acknowledge_for_principal(
    state: &HubState,
    principal: &Principal,
    id: String,
) -> Result<OwnerNoticeView, HubError> {
    if principal.read_only {
        return Err(HubError::Unauthorized);
    }
    let store = state.store.clone();
    let owner = principal.owner.clone();
    let repository = principal.repository.clone();
    let repository_id = principal.repository_id.ok_or(HubError::Unauthorized)?;
    Ok(blocking(state, move || {
        store.owner_notice_recipient_status(
            &WorkLifecycleReader::Person {
                owner: &owner,
                repository: &repository,
                repository_id,
            },
            &id,
            OwnerNoticeRecipientStatus::Acknowledged,
            unix_now(),
        )
    })
    .await?
    .into())
}

async fn acknowledge(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    Path(id): Path<String>,
) -> Result<Json<OwnerNoticeView>, ApiError> {
    Ok(Json(
        acknowledge_for_principal(&state, &principal, id).await?,
    ))
}
