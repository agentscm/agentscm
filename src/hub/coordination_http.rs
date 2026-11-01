//! Agent inbox delivery and read-only repository coordination inspection.
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, Semaphore};
use tokio::time::Instant;

use super::coordination::{
    CoordinationAck, CoordinationInbox, CoordinationInboxQuery, CoordinationListQuery,
    CoordinationReply, CoordinationRequest, CoordinationResolve, CoordinationResponseKind,
    CoordinationWithdraw,
};
use super::handoff::{Answered, Asked, HandoffParty, Via};
use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::login::HumanSession;
use super::store::unix_now;
use super::{HubError, HubState, Result};

pub(super) static CHANGED: Notify = Notify::const_new();
static WAITERS: Semaphore = Semaphore::const_new(32);

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/standup/coordination", get(list))
        .route("/v1/hub/standup/inbox", get(inbox))
        .route("/v1/hub/standup/coordination/ack", post(ack))
        .route("/v1/hub/standup/coordination/reply", post(reply))
        .route("/v1/hub/standup/coordination/resolve", post(resolve))
        .route("/v1/hub/standup/coordination/withdraw", post(withdraw))
        .route("/v1/hub/session/handoffs", get(person_handoffs))
        .route("/v1/hub/session/coordination/reply", post(person_reply))
        .route("/v1/hub/session/handoff/answer", post(person_answer))
        .route("/v1/hub/session/handoff/ask", post(person_ask))
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

// A signed-in person answering for their own agent from the Hub's web pages. The cookie sign-in
// and its CSRF token are checked by `HumanSession`; each route then requires the named agent
// token to be an active, non-read-only token of that same person. The store applies its own
// rules on top (the token must lead the target Work, hold the file, and so on), and records
// that a person acted through the Hub rather than the agent itself.

/// A refusal people read: the store's `Unauthorized` is a 403 here, never the 401 that would
/// end their sign-in, and its `Invalid` reasons are the ones meant for people.
fn person_error(error: HubError) -> ApiError {
    match error {
        HubError::Unauthorized => not_your_agent(),
        HubError::Invalid(message) => ApiError::invalid().with_message(message),
        other => other.into(),
    }
}

fn not_your_agent() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "not_your_agent")
        .with_message("That agent is not yours, or it is no longer connected")
}

/// Whether `token` is an active agent token of `owner` that can act (not read-only).
fn owns_agent(store: &dyn super::Store, owner: &str, token: &str, now: i64) -> Result<bool> {
    Ok(store
        .list_agent_tokens(owner)?
        .iter()
        .any(|t| t.id == token && !t.revoked && !t.read_only && t.expires_at > now))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonReply {
    /// The agent token that leads the target Work.
    token: String,
    request_id: String,
    operation_id: String,
    target_work_id: String,
    session: String,
    expected_updated_seq: i64,
    kind: CoordinationResponseKind,
    note: Option<String>,
}

async fn person_reply(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<PersonReply>,
) -> std::result::Result<Json<CoordinationRequest>, ApiError> {
    let owner = human.web_user()?.id.clone();
    let store = state.store.clone();
    let result = blocking(&state, move || {
        let now = unix_now();
        if !owns_agent(&*store, &owner, &body.token, now)? {
            return Err(HubError::Unauthorized);
        }
        let reply = CoordinationReply {
            request_id: body.request_id,
            operation_id: body.operation_id,
            target_work_id: body.target_work_id,
            session: body.session,
            expected_updated_seq: body.expected_updated_seq,
            kind: body.kind,
            note: body.note,
            via_hub: true,
        };
        store.coordination_reply(&body.token, &reply, now)
    })
    .await
    .map_err(person_error)?;
    CHANGED.notify_waiters();
    Ok(Json(result))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonAnswer {
    /// The agent token holding the file.
    token: String,
    session: Option<String>,
    path: String,
    accept: bool,
    reason: Option<String>,
}

async fn person_answer(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<PersonAnswer>,
) -> std::result::Result<Json<Value>, ApiError> {
    let owner = human.web_user()?.id.clone();
    let store = state.store.clone();
    let answered = blocking(&state, move || {
        let now = unix_now();
        if !owns_agent(&*store, &owner, &body.token, now)? {
            return Err(HubError::Unauthorized);
        }
        store.answer_handoff_via(
            &body.token,
            body.session.as_deref(),
            &body.path,
            (body.accept, body.reason.as_deref()),
            Via::Hub,
            now,
        )
    })
    .await
    .map_err(person_error)?;
    let (outcome, requester) = match answered {
        Answered::Granted { requester } => ("granted", Some(requester)),
        Answered::Declined { requester } => ("declined", Some(requester)),
        Answered::NoRequest => ("no_open_request", None),
        Answered::NotHeld => ("not_held", None),
    };
    Ok(Json(json!({"outcome": outcome,
        "requester": requester.map(|(agent, login)| json!({"agent": agent, "login": login}))})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PersonAsk {
    /// The person's own agent token that wants the file.
    token: String,
    session: Option<String>,
    path: String,
    note: Option<String>,
}

async fn person_ask(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<PersonAsk>,
) -> std::result::Result<Json<Value>, ApiError> {
    let owner = human.web_user()?.id.clone();
    let store = state.store.clone();
    let asked = blocking(&state, move || {
        let now = unix_now();
        if !owns_agent(&*store, &owner, &body.token, now)? {
            return Err(HubError::Unauthorized);
        }
        store.ask_handoff_via(
            &body.token,
            body.session.as_deref(),
            &body.path,
            body.note.as_deref(),
            Via::Hub,
            now,
        )
    })
    .await
    .map_err(person_error)?;
    let (outcome, holder) = match asked {
        Asked::Requested { holder } => ("requested", Some(holder)),
        Asked::AlreadyAsked { holder } => ("already_asked", Some(holder)),
        Asked::NotHeld => ("not_held", None),
        Asked::Yours => ("yours", None),
        Asked::TooMany => ("too_many", None),
    };
    Ok(Json(json!({"outcome": outcome,
        "holder": holder.map(|h| json!({"agent": h.agent, "login": h.owner.login}))})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandoffQuery {
    repository: String,
}

/// How many of a person's newest handoffs the page reads.
const PERSON_HANDOFFS: usize = 50;

/// One side of a handoff. The token and session, which the page needs to answer, are given only
/// for the person's own agent.
fn party_json(party: &HandoffParty, me: &str) -> Value {
    let mut out = json!({"agent": party.agent, "login": party.login, "mine": party.owner == me});
    if party.owner == me {
        out["token"] = json!(party.token);
        out["session"] = json!(party.session);
    }
    out
}

/// The signed-in person's recent file handoffs in one repository, as either side.
async fn person_handoffs(
    State(state): State<HubState>,
    human: HumanSession,
    query: std::result::Result<Query<HandoffQuery>, QueryRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let me = human.web_user()?.id.clone();
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let store = state.store.clone();
    let owner = me.clone();
    let rows = blocking(&state, move || {
        store.handoffs_of_owner(&owner, &query.repository, unix_now(), PERSON_HANDOFFS)
    })
    .await
    .map_err(person_error)?;
    let handoffs: Vec<Value> = rows
        .iter()
        .map(|h| {
            json!({"id": h.id, "path": h.path, "state": h.state, "note": h.note,
                "reply": h.reply, "created_at": h.created_at, "decided_at": h.decided_at,
                "holder_told": h.holder_told, "requester_told": h.requester_told,
                "asked_via_hub": h.asked_via_hub, "answered_via_hub": h.answered_via_hub,
                "answered_via_slack": h.answered_via_slack,
                "requester": party_json(&h.requester, &me), "holder": party_json(&h.holder, &me)})
        })
        .collect();
    Ok(Json(json!({"handoffs": handoffs})))
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
