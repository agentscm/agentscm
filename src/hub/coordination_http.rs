//! Agent inbox delivery and read-only repository coordination inspection.
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Query, State};
use axum::routing::{get, post};
use serde::Deserialize;
use tokio::sync::{Notify, Semaphore};
use tokio::time::Instant;

use super::coordination::{
    CoordinationAck, CoordinationInbox, CoordinationInboxQuery, CoordinationListQuery,
    CoordinationReply, CoordinationRequest, CoordinationResolve, CoordinationWithdraw,
};
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::store::unix_now;
use super::{HubError, HubState, Result};

pub(super) static CHANGED: Notify = Notify::const_new();
static WAITERS: Semaphore = Semaphore::const_new(256);

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/standup/coordination", get(list))
        .route("/v1/hub/standup/inbox", get(inbox))
        .route("/v1/hub/standup/coordination/ack", post(ack))
        .route("/v1/hub/standup/coordination/reply", post(reply))
        .route("/v1/hub/standup/coordination/resolve", post(resolve))
        .route("/v1/hub/standup/coordination/withdraw", post(withdraw))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InboxQuery {
    session: Option<String>,
    after: Option<i64>,
    through: Option<i64>,
    #[serde(default = "page_limit")]
    limit: usize,
    #[serde(default)]
    wait_seconds: u64,
}

const fn page_limit() -> usize {
    100
}

/// Notification is a latency optimization. Database replay remains authoritative across processes.
pub(super) async fn poll_inbox(
    state: &HubState,
    token: String,
    query: CoordinationInboxQuery,
    wait_seconds: u64,
) -> Result<CoordinationInbox> {
    if wait_seconds > 25 || (wait_seconds > 0 && query.through.is_some()) {
        return Err(HubError::Invalid(
            "wait_seconds must be 0..25 and requires a fresh watermark".into(),
        ));
    }
    let permit = WAITERS.try_acquire().ok();
    let deadline =
        Instant::now() + Duration::from_secs(if permit.is_some() { wait_seconds } else { 0 });
    loop {
        // Register before reading so a local commit cannot be missed between read and wait.
        let notified = CHANGED.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let store = state.store.clone();
        let token = token.clone();
        let query = query.clone();
        let found = blocking(state, move || {
            store.coordination_inbox(&token, &query, unix_now())
        })
        .await;
        match found {
            // A local commit wakes every waiter. Retry while its wait remains
            // open instead of turning a temporary store-permit shortage into 503.
            Err(HubError::Busy) if Instant::now() < deadline => {}
            found => {
                let page = found?;
                if !page.requests.is_empty() || !page.complete || Instant::now() >= deadline {
                    return Ok(page);
                }
            }
        }
        tokio::select! {
            _ = &mut notified => {},
            _ = tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_secs(1))) => {},
        }
    }
}

async fn list(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<CoordinationListQuery>, QueryRejection>,
) -> std::result::Result<Json<CoordinationInbox>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    Ok(Json(
        blocking(&state, move || {
            store.coordination_list(&principal.token_id, &query, unix_now())
        })
        .await?,
    ))
}

async fn inbox(
    State(state): State<HubState>,
    Reader(principal): Reader,
    query: std::result::Result<Query<InboxQuery>, QueryRejection>,
) -> std::result::Result<Json<CoordinationInbox>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let page = poll_inbox(
        &state,
        principal.token_id,
        CoordinationInboxQuery {
            session: query.session,
            after: query.after,
            through: query.through,
            limit: query.limit,
        },
        query.wait_seconds,
    )
    .await?;
    Ok(Json(page))
}

async fn ack(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<CoordinationAck>,
) -> std::result::Result<Json<CoordinationRequest>, ApiError> {
    let store = state.store.clone();
    let result = blocking(&state, move || {
        store.coordination_ack(&principal.token_id, &request, unix_now())
    })
    .await?;
    CHANGED.notify_waiters();
    Ok(Json(result))
}

async fn reply(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<CoordinationReply>,
) -> std::result::Result<Json<CoordinationRequest>, ApiError> {
    let store = state.store.clone();
    let result = blocking(&state, move || {
        store.coordination_reply(&principal.token_id, &request, unix_now())
    })
    .await?;
    CHANGED.notify_waiters();
    Ok(Json(result))
}

async fn resolve(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<CoordinationResolve>,
) -> std::result::Result<Json<CoordinationRequest>, ApiError> {
    let store = state.store.clone();
    let result = blocking(&state, move || {
        store.coordination_resolve(&principal.token_id, &request, unix_now())
    })
    .await?;
    CHANGED.notify_waiters();
    Ok(Json(result))
}

async fn withdraw(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    JsonBody(request): JsonBody<CoordinationWithdraw>,
) -> std::result::Result<Json<CoordinationRequest>, ApiError> {
    let store = state.store.clone();
    let result = blocking(&state, move || {
        store.coordination_withdraw(&principal.token_id, &request, unix_now())
    })
    .await?;
    CHANGED.notify_waiters();
    Ok(Json(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::LocalBackend;
    use crate::hub::store::{self, ForgeIdentity, mint_agent_token};
    use std::sync::Arc;

    #[tokio::test]
    async fn long_poll_retries_temporary_store_saturation() {
        let dir = tempfile::tempdir().unwrap();
        let store = store::open(&format!(
            "sqlite://{}",
            dir.path().join("hub.sqlite").display()
        ))
        .unwrap();
        let owner = store
            .upsert_user(
                &ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                "contributor",
            )
            .unwrap();
        let (token, _) =
            mint_agent_token(&*store, &owner.id, "worker", "acme/widgets", 3600).unwrap();
        let state = HubState::new(
            store,
            Arc::new(LocalBackend::open(dir.path().join("objects")).unwrap()),
        );
        let held = state
            .permits
            .clone()
            .acquire_many_owned(super::super::MAX_CONCURRENT_STORE_CALLS as u32)
            .await
            .unwrap();
        let waiter = tokio::spawn(async move {
            poll_inbox(
                &state,
                token,
                CoordinationInboxQuery {
                    session: Some("session".into()),
                    ..Default::default()
                },
                1,
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !waiter.is_finished(),
            "temporary saturation must not finish a live poll with Busy"
        );
        drop(held);
        CHANGED.notify_waiters();
        let page = tokio::time::timeout(Duration::from_secs(3), waiter)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(page.requests.is_empty() && page.complete);
    }
}
