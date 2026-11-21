//! Authenticated native transport operations; none of these routes renew a Work lease.
use axum::{Json, Router, extract::State, routing::post};
use serde::Deserialize;
use serde::Serialize;

use super::HubState;
use super::http::{ApiError, Authenticated, JsonBody, blocking};
use super::notices::{DependencyRefresh, NoticeTransportReport};
use super::session_lifecycle::{LifecycleEnd, LifecycleRegister};
use super::store::unix_now;

pub const INCARNATION_HEADER: &str = "x-agentscm-incarnation";
pub const SESSION_HEADER: &str = "x-agentscm-session";

pub(super) fn offer_on_call(
    store: &dyn super::store::Store,
    principal: &super::store::Principal,
    session: &str,
    incarnation: &str,
    now: i64,
) -> super::Result<Option<super::notices::NoticeOffer>> {
    let mut after = None;
    for _ in 0..16 {
        let page = store.notice_refresh(
            &principal.token_id,
            &DependencyRefresh {
                session: session.into(),
                incarnation: incarnation.into(),
                after,
                limit: 100,
            },
            now,
        )?;
        if !page.coverage_complete {
            return Err(super::HubError::Unavailable(
                "Dependency refresh coverage is incomplete.".into(),
            ));
        }
        if page.complete {
            let domain = store.session_name_for(&principal.token_id, Some(session), now)?;
            super::agent_messages::record_current_sources(store, principal, &domain, now)?;
            return store.notice_offer(
                &principal.token_id,
                &super::notices::NoticeOfferRequest {
                    session: session.into(),
                    incarnation: incarnation.into(),
                    automatic: false,
                },
                now,
            );
        }
        after = page.next_after;
        if after.is_none() {
            break;
        }
    }
    Err(super::HubError::Unavailable(
        "Dependency refresh exceeds this call's bounded pages.".into(),
    ))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeMetadata {
    pub session: String,
    pub incarnation: String,
}

pub fn incarnation(
    headers: &axum::http::HeaderMap,
    session: Option<&str>,
) -> super::Result<Option<String>> {
    let id = headers.get(INCARNATION_HEADER);
    let native = headers.get(SESSION_HEADER);
    match (id, native) {
        (None, None) => Ok(None),
        (Some(id), Some(native)) => {
            let id = id.to_str().map_err(|_| {
                super::HubError::invalid_or_changed("Invalid native session metadata.".into())
            })?;
            let native = native.to_str().map_err(|_| {
                super::HubError::invalid_or_changed("Invalid native session metadata.".into())
            })?;
            super::session::check_name(native)?;
            if id.is_empty() || id.len() > 128 || session.is_some_and(|session| session != native) {
                return Err(super::HubError::invalid_or_changed(
                    "Native session metadata changed.".into(),
                ));
            }
            Ok(Some(id.to_owned()))
        }
        _ => Err(super::HubError::invalid_or_changed(
            "Incomplete native session metadata.".into(),
        )),
    }
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/cooperation/register", post(register))
        .route("/v1/hub/cooperation/end", post(end))
        .route("/v1/hub/cooperation/state", post(state))
        .route("/v1/hub/cooperation/refresh", post(refresh))
        .route("/v1/hub/cooperation/transport", post(transport))
}

async fn register(
    State(hub): State<HubState>,
    Authenticated(who): Authenticated,
    JsonBody(request): JsonBody<LifecycleRegister>,
) -> Result<Json<super::session_lifecycle::LifecycleView>, ApiError> {
    let store = hub.store.clone();
    Ok(Json(
        blocking(&hub, move || {
            store.lifecycle_register(&who.token_id, &request, unix_now())
        })
        .await?,
    ))
}

async fn end(
    State(hub): State<HubState>,
    Authenticated(who): Authenticated,
    JsonBody(request): JsonBody<LifecycleEnd>,
) -> Result<Json<super::session_lifecycle::LifecycleEndResult>, ApiError> {
    let store = hub.store.clone();
    let result = blocking(&hub, move || {
        store.lifecycle_end(&who.token_id, &request, unix_now())
    })
    .await?;
    super::coordination_http::CHANGED.notify_waiters();
    Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateRequest {
    session: String,
}

async fn state(
    State(hub): State<HubState>,
    Authenticated(who): Authenticated,
    JsonBody(request): JsonBody<StateRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let store = hub.store.clone();
    Ok(Json(
        blocking(&hub, move || {
            let now = unix_now();
            let lifecycle = store.lifecycle_get(&who.token_id, &request.session, now)?;
            let work = store.lifecycle_fences(&who.token_id, &request.session, now)?;
            Ok(serde_json::json!({"lifecycle": lifecycle, "work": work}))
        })
        .await?,
    ))
}

async fn refresh(
    State(hub): State<HubState>,
    Authenticated(who): Authenticated,
    JsonBody(request): JsonBody<DependencyRefresh>,
) -> Result<Json<super::notices::NoticeRefreshPage>, ApiError> {
    let store = hub.store.clone();
    Ok(Json(
        blocking(&hub, move || {
            store.notice_refresh(&who.token_id, &request, unix_now())
        })
        .await?,
    ))
}

async fn transport(
    State(hub): State<HubState>,
    Authenticated(who): Authenticated,
    JsonBody(request): JsonBody<NoticeTransportReport>,
) -> Result<Json<super::notices::NoticeReceipt>, ApiError> {
    let store = hub.store.clone();
    Ok(Json(
        blocking(&hub, move || {
            store.notice_transport(&who.token_id, &request, unix_now())
        })
        .await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    #[test]
    fn native_metadata_is_optional_but_never_partial_or_cross_session() {
        let mut headers = HeaderMap::new();
        assert_eq!(incarnation(&headers, Some("session")).unwrap(), None);
        headers.insert(INCARNATION_HEADER, "inc-current".parse().unwrap());
        assert!(incarnation(&headers, Some("session")).is_err());
        headers.insert(SESSION_HEADER, "session".parse().unwrap());
        assert_eq!(
            incarnation(&headers, Some("session")).unwrap().as_deref(),
            Some("inc-current")
        );
        assert!(incarnation(&headers, Some("other-session")).is_err());
        headers.insert(INCARNATION_HEADER, "".parse().unwrap());
        assert!(incarnation(&headers, Some("session")).is_err());
    }
}
