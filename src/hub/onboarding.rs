//! Short-lived onboarding observations, never an authorization or host attestation.
//! Restarting the Hub deliberately loses proof. No provider output or credentials are retained.
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::FromRequestParts;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::http::{ApiError, Authenticated, JsonBody, Reader, blocking};
use super::session::check_name;
use super::store::{Principal, unix_now};
use super::{HubError, HubState, Result};

pub const CLIENT_HEADER: &str = "x-agentscm-client";
const TTL: i64 = 15 * 60;
const MAX_ROWS: usize = 256;
const MAX_TOKEN_ROWS: usize = 16;
const MAX_REPO_ROWS: usize = 64;
pub const PENDING_SESSION: &str = "agentscm-setup-pending";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Agent {
    Claude,
    Codex,
    Cursor,
    Copilot,
    Gemini,
}

impl Agent {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "claude" => Some(Self::Claude),
            "codex" => Some(Self::Codex),
            "cursor" => Some(Self::Cursor),
            "copilot" => Some(Self::Copilot),
            "gemini" => Some(Self::Gemini),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Blocker {
    Login,
    Trust,
    Quota,
    Unsupported,
    Unknown,
}

impl Blocker {
    /// Classify locally captured host diagnostics. Ambiguous output remains unknown;
    /// a clean login/status command is never evidence of model access or hook execution.
    pub fn detect(text: &str) -> Self {
        let text = text.to_ascii_lowercase();
        let login_missing = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .is_some_and(|value| {
                ["loggedin", "logged_in", "authenticated"]
                    .iter()
                    .any(|field| value[*field] == false)
            });
        if [
            "unsupported_client",
            "unsupported client",
            "not supported for your account",
        ]
        .iter()
        .any(|p| text.contains(p))
        {
            Self::Unsupported
        } else if [
            "quota",
            "rate_limit",
            "rate limit",
            "usage limit",
            "out of credits",
            "limit reached",
            "limit hit",
            "hit your limit",
            "resource_exhausted",
            "429",
        ]
        .iter()
        .any(|p| text.contains(p))
        {
            Self::Quota
        } else if login_missing
            || [
                "not logged in",
                "not signed in",
                "login required",
                "sign in required",
                "authentication required",
                "invalid_grant",
                "unauthenticated",
                "not authenticated",
                "please log in",
            ]
            .iter()
            .any(|p| text.contains(p))
        {
            Self::Login
        } else if [
            "untrusted",
            "trust required",
            "approval required",
            "needs approval",
            "pending approval",
            "requires approval",
            "need review",
            "not approved",
        ]
        .iter()
        .any(|p| text.contains(p))
        {
            Self::Trust
        } else {
            Self::Unknown
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupCall {
    pub agent: Agent,
    pub session: String,
    pub blocker: Option<Blocker>,
}

#[derive(Clone, Serialize)]
pub struct Status {
    pub key: String,
    pub agent: Agent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    pub name: String,
    pub owner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    pub state: &'static str,
    pub blocker: Option<Blocker>,
    pub mcp_verified: bool,
    pub native_verified: bool,
    pub expires_at: i64,
    pub next_action: &'static str,
}

#[derive(Clone)]
struct Row {
    principal: Principal,
    client: String,
    call: SetupCall,
    mcp: Option<i64>,
    native: Option<i64>,
    updated: i64,
    closed: bool,
}

impl Row {
    fn status(&self, now: i64) -> Status {
        let fresh = |at: Option<i64>| at.is_some_and(|at| at <= now && now - at < TTL);
        let mcp = !self.closed && fresh(self.mcp);
        let native = !self.closed && fresh(self.native);
        let blocker = self.call.blocker;
        let ready = blocker.is_none() && mcp && native;
        let action = match blocker {
            Some(Blocker::Login) => {
                "Sign in using this coding agent's own login flow, then resume setup. Forge sign-in does not sign in your coding agent."
            }
            Some(Blocker::Trust) => {
                "Review the generated AgentSCM plugin/hooks and MCP server in your host, approve them yourself, then start a fresh session. Never bypass trust."
            }
            Some(Blocker::Quota) => {
                "Your coding agent reported a usage limit. Wait for its reset or use an existing authorized plan. Resume verification afterward; do not switch billing or buy credits automatically."
            }
            Some(Blocker::Unsupported) => {
                "This client/account combination was rejected. Check supported access with the provider. Do not silently switch credentials or billing."
            }
            Some(Blocker::Unknown) => {
                "Inspect your host's login, MCP and hook status. Report only the blocker category; do not share credentials or raw diagnostics."
            }
            None if !mcp => {
                "Call agentscm_setup_status through your agent's native AgentSCM MCP connection with this agent and its actual native session ID. CLI checks do not verify MCP."
            }
            None if !native => {
                "Use a native file-edit tool on a disposable repository file in this same session, then call agentscm_setup_status again. Session-start hooks and shell writes do not qualify. Cursor observes edits afterward."
            }
            None => {
                "MCP and a native edit-hook check were observed for this connection and session. Continue work; this is coordination evidence, not a guarantee of future quota or complete tool coverage."
            }
        };
        let expires = [self.mcp, self.native]
            .into_iter()
            .flatten()
            .min()
            .unwrap_or(self.updated)
            + TTL;
        Status {
            key: format!(
                "{:x}",
                Sha256::digest(
                    serde_json::to_vec(&(
                        &self.principal.token_id,
                        &self.client,
                        self.call.agent,
                        &self.call.session
                    ))
                    .unwrap_or_default()
                )
            ),
            agent: self.call.agent,
            session: Some(self.call.session.clone()),
            token: Some(self.principal.token_id.clone()),
            name: self.principal.name.clone(),
            owner: self.principal.owner.login.clone(),
            client: Some(self.client.clone()),
            state: if ready {
                "ready"
            } else if blocker.is_some() {
                "blocked"
            } else {
                "unverified"
            },
            blocker,
            mcp_verified: mcp,
            native_verified: native,
            expires_at: expires.min(self.principal.expires_at),
            next_action: action,
        }
    }
}

type Key = (String, String, Agent, String);
#[derive(Default)]
pub struct Observations(Mutex<BTreeMap<Key, Row>>, AtomicU64);

/// Capture before authentication/body waits. A Hub-wide conservative fence rejects
/// in-flight proof if any lifecycle/blocker invalidation occurred while it waited.
/// Unrelated activity can require retrying verification, never create a false Ready.
pub(super) struct Epoch(pub u64);
impl FromRequestParts<HubState> for Epoch {
    type Rejection = Infallible;
    async fn from_request_parts(
        _: &mut Parts,
        state: &HubState,
    ) -> std::result::Result<Self, Self::Rejection> {
        Ok(Self(state.onboarding.epoch()))
    }
}

pub fn client(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all(CLIENT_HEADER).iter();
    let value = values.next()?.to_str().ok()?;
    (values.next().is_none()
        && value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()))
    .then_some(value)
}

impl Observations {
    pub(super) fn epoch(&self) -> u64 {
        self.1.load(Ordering::SeqCst)
    }
    fn invalidate(&self) {
        let _ = self
            .1
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                Some(n.saturating_add(1))
            });
    }
    pub(super) fn restart(
        &self,
        principal: &Principal,
        client: Option<&str>,
        agent: &str,
        session: Option<&str>,
        epoch: u64,
    ) {
        let (Some(client), Some(agent), Some(session)) = (client, Agent::from_name(agent), session)
        else {
            return;
        };
        self.lifecycle(
            principal,
            client,
            SetupCall {
                agent,
                session: session.into(),
                blocker: None,
            },
            false,
            Some(epoch),
            unix_now(),
        );
    }
    fn lifecycle(
        &self,
        principal: &Principal,
        client: &str,
        call: SetupCall,
        closed: bool,
        epoch: Option<u64>,
        now: i64,
    ) {
        if check_name(&call.session).is_err() {
            return;
        }
        let result =
            self.update_fenced(principal, client, call.clone(), (false, false), epoch, now);
        if let Ok(mut rows) = self.0.lock() {
            let may_open = result.is_ok()
                && epoch.is_none_or(|epoch| epoch != u64::MAX && epoch == self.epoch());
            self.invalidate();
            if let Some(row) = rows.get_mut(&(
                principal.token_id.clone(),
                client.into(),
                call.agent,
                call.session,
            )) {
                row.mcp = None;
                row.native = None;
                // A stale Start still invalidates its own pre-restart proof, but
                // cannot reopen a row after an intervening end/blocker.
                row.closed = closed || !may_open;
            }
        }
    }
    fn update(
        &self,
        principal: &Principal,
        client: &str,
        call: SetupCall,
        mcp: bool,
        native: bool,
        now: i64,
    ) -> Result<Status> {
        self.update_fenced(principal, client, call, (mcp, native), None, now)
    }
    fn update_fenced(
        &self,
        principal: &Principal,
        client: &str,
        call: SetupCall,
        proof: (bool, bool),
        epoch: Option<u64>,
        now: i64,
    ) -> Result<Status> {
        let (mcp, native) = proof;
        check_name(&call.session)?;
        if call.session == PENDING_SESSION && (mcp || native) {
            return Err(HubError::Invalid(
                "Use the actual native session ID to verify setup".into(),
            ));
        }
        if principal.read_only || now >= principal.expires_at {
            return Err(HubError::Unauthorized);
        }
        let mut rows = self.0.lock().map_err(|_| HubError::Busy)?;
        if epoch.is_some_and(|epoch| epoch == u64::MAX || epoch != self.epoch()) {
            return Err(HubError::Invalid("Setup changed while verification was in flight; retry in the current native session".into()));
        }
        rows.retain(|_, row| {
            row.updated <= now && now - row.updated < TTL && row.principal.expires_at > now
        });
        if call.session == PENDING_SESSION && call.blocker.is_some() {
            for row in rows.values_mut().filter(|row| {
                row.principal.token_id == principal.token_id
                    && row.client == client
                    && row.call.agent == call.agent
            }) {
                row.call.blocker = call.blocker;
                row.mcp = None;
                row.native = None;
            }
        }
        if call.blocker.is_some() {
            self.invalidate();
        }
        let key = (
            principal.token_id.clone(),
            client.to_owned(),
            call.agent,
            call.session.clone(),
        );
        if !rows.contains_key(&key) {
            let owned: Vec<_> = rows
                .iter()
                .filter(|(_, row)| row.principal.token_id == principal.token_id)
                .collect();
            if owned.len() >= MAX_TOKEN_ROWS {
                let oldest = owned
                    .into_iter()
                    .min_by_key(|(_, row)| row.updated)
                    .map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    rows.remove(&oldest);
                }
            }
            if rows.len() >= MAX_ROWS
                || rows
                    .values()
                    .filter(|row| same_repository(&row.principal, principal))
                    .count()
                    >= MAX_REPO_ROWS
            {
                return Err(HubError::Busy);
            }
        }
        let row = rows.entry(key).or_insert_with(|| Row {
            principal: principal.clone(),
            client: client.into(),
            call: call.clone(),
            mcp: None,
            native: None,
            updated: now,
            closed: true,
        });
        if row.closed && (mcp || native) {
            return Err(HubError::Invalid(
                "Native session start is not verified; restart the native session before verifying setup".into(),
            ));
        }
        row.updated = now;
        if call.blocker.is_some() {
            row.call.blocker = call.blocker;
            row.mcp = None;
            row.native = None;
        } else {
            // A blocker is cleared only by new proof of BOTH transports, not a status report.
            if mcp {
                row.mcp = Some(now);
            }
            if native {
                row.native = Some(now);
            }
            if row.mcp.is_some_and(|at| now - at < TTL)
                && row.native.is_some_and(|at| now - at < TTL)
            {
                row.call.blocker = None;
            }
        }
        let status = row.status(now);
        if status.state == "ready" {
            rows.remove(&(
                principal.token_id.clone(),
                client.into(),
                call.agent,
                PENDING_SESSION.into(),
            ));
        }
        Ok(status)
    }

    pub(super) fn native(
        &self,
        principal: &Principal,
        client: Option<&str>,
        agent: &str,
        session: Option<&str>,
        end: bool,
        epoch: u64,
    ) {
        let (Some(client), Some(agent), Some(session)) = (client, Agent::from_name(agent), session)
        else {
            return;
        };
        if end {
            self.lifecycle(
                principal,
                client,
                SetupCall {
                    agent,
                    session: session.into(),
                    blocker: None,
                },
                true,
                None,
                unix_now(),
            );
        } else {
            let _ = self.update_fenced(
                principal,
                client,
                SetupCall {
                    agent,
                    session: session.into(),
                    blocker: None,
                },
                (false, true),
                Some(epoch),
                unix_now(),
            );
        }
    }
}

fn same_repository(left: &Principal, right: &Principal) -> bool {
    left.repository == right.repository
        && left.repository_id == right.repository_id
        && left.owner.identity.provider == right.owner.identity.provider
        && left.owner.identity.host == right.owner.identity.host
}

pub fn routes() -> Router<HubState> {
    Router::new().route("/v1/hub/onboarding", get(list).post(report))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    agent: Option<Agent>,
    session: Option<String>,
}

async fn list(
    State(state): State<HubState>,
    Reader(principal): Reader,
    headers: HeaderMap,
    Query(query): Query<ListQuery>,
) -> std::result::Result<Json<Vec<Status>>, ApiError> {
    let now = unix_now();
    let selected_client = client(&headers).map(str::to_owned);
    let rows: Vec<Row> = state
        .onboarding
        .0
        .lock()
        .map_err(|_| HubError::Busy)?
        .values()
        .filter(|row| {
            same_repository(&row.principal, &principal)
                && (!row.closed || row.call.blocker.is_some())
                && row.updated <= now
                && now - row.updated < TTL
                && row.principal.expires_at > now
                && query.agent.is_none_or(|agent| row.call.agent == agent)
                && query
                    .session
                    .as_ref()
                    .is_none_or(|session| &row.call.session == session)
                && selected_client.as_ref().is_none_or(|client| {
                    row.client == *client && row.principal.token_id == principal.token_id
                })
        })
        .cloned()
        .collect();
    let store = state.store.clone();
    // Recheck each observed token's current revocation and owner status before showing readiness.
    let statuses = blocking(&state, move || {
        let mut out = Vec::new();
        let mut valid = BTreeMap::new();
        for row in rows {
            let owner = &row.principal.owner;
            if !valid.contains_key(&owner.id) {
                let tokens: Vec<_> = if store
                    .user_by_id(&owner.id)?
                    .is_some_and(|user| !user.disabled)
                {
                    store
                        .list_agent_tokens(&owner.id)?
                        .into_iter()
                        .filter(|token| !token.revoked && token.expires_at > now)
                        .map(|token| token.id)
                        .collect()
                } else {
                    Vec::new()
                };
                valid.insert(owner.id.clone(), tokens);
            }
            if valid[&owner.id].contains(&row.principal.token_id) {
                let mut status = row.status(now);
                if selected_client.is_none() {
                    status.session = None;
                    status.token = None;
                    status.client = None;
                }
                out.push(status);
            }
        }
        Ok(out)
    })
    .await?;
    Ok(Json(statuses))
}

async fn report(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    headers: HeaderMap,
    JsonBody(call): JsonBody<SetupCall>,
) -> std::result::Result<Json<Status>, ApiError> {
    let client = client(&headers).ok_or_else(ApiError::invalid)?;
    // HTTP reports can describe blockers, but can never manufacture transport proof.
    Ok(Json(state.onboarding.update(
        &principal,
        client,
        call,
        false,
        false,
        unix_now(),
    )?))
}

pub(super) fn mcp(
    state: &HubState,
    principal: &Principal,
    client: Option<&str>,
    call: SetupCall,
    epoch: u64,
) -> Result<Status> {
    let client = client.ok_or_else(|| {
        HubError::Invalid(
            "Restart the checkout-bound AgentSCM MCP transport to verify setup".into(),
        )
    })?;
    state.onboarding.update_fenced(
        principal,
        client,
        call,
        (true, false),
        Some(epoch),
        unix_now(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::store::{ForgeIdentity, User};

    fn principal(token: &str) -> Principal {
        Principal {
            token_id: token.into(),
            name: "agent".into(),
            repository: "acme/widgets".into(),
            repository_id: None,
            expires_at: 100_000,
            read_only: false,
            owner: User {
                id: "user".into(),
                identity: ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                login: "member".into(),
                disabled: false,
            },
        }
    }
    fn call(session: &str, blocker: Option<Blocker>) -> SetupCall {
        SetupCall {
            agent: Agent::Codex,
            session: session.into(),
            blocker,
        }
    }
    fn start(
        observations: &Observations,
        principal: &Principal,
        client: &str,
        session: &str,
        now: i64,
    ) {
        observations.lifecycle(
            principal,
            client,
            call(session, None),
            false,
            Some(observations.epoch()),
            now,
        );
    }

    #[test]
    fn only_both_recent_transports_in_the_same_binding_and_session_are_ready() {
        let observations = Observations::default();
        let p = principal("one");
        start(&observations, &p, "root-a", "s", 100);
        assert_eq!(
            observations
                .update(&p, "root-a", call("s", None), false, false, 100)
                .unwrap()
                .state,
            "unverified"
        );
        observations
            .update(&p, "root-a", call("s", None), true, false, 100)
            .unwrap();
        for (p, root, session) in [
            (&p, "root-b", "s"),
            (&p, "root-a", "other"),
            (&principal("two"), "root-a", "s"),
        ] {
            start(&observations, p, root, session, 101);
            assert_eq!(
                observations
                    .update(p, root, call(session, None), false, true, 101)
                    .unwrap()
                    .state,
                "unverified"
            );
        }
        let ready = observations
            .update(&p, "root-a", call("s", None), false, true, 101)
            .unwrap();
        assert_eq!(ready.state, "ready");
        let row = observations
            .0
            .lock()
            .unwrap()
            .values()
            .find(|r| {
                r.principal.token_id == "one" && r.client == "root-a" && r.call.session == "s"
            })
            .unwrap()
            .clone();
        assert_eq!(row.status(100 + TTL).state, "unverified");
        assert_eq!(row.status(99).state, "unverified");
        start(&observations, &p, "root-a", "s", 102);
        assert!(
            !observations
                .0
                .lock()
                .unwrap()
                .values()
                .any(|r| r.client == "root-a"
                    && r.call.session == "s"
                    && r.principal.token_id == "one"
                    && r.status(102).state == "ready")
        );
    }

    #[test]
    fn blockers_invalidate_proof_and_need_both_transports_again() {
        let observations = Observations::default();
        let p = principal("one");
        start(&observations, &p, "a", "s", 100);
        observations
            .update(&p, "a", call("s", None), true, true, 100)
            .unwrap();
        let blocked = observations
            .update(&p, "a", call("s", Some(Blocker::Quota)), false, false, 101)
            .unwrap();
        assert_eq!(blocked.state, "blocked");
        assert!(!blocked.mcp_verified && !blocked.native_verified);
        assert_eq!(
            observations
                .update(&p, "a", call("s", None), false, false, 102)
                .unwrap()
                .state,
            "blocked"
        );
        assert_eq!(
            observations
                .update(&p, "a", call("s", None), true, false, 103)
                .unwrap()
                .state,
            "blocked"
        );
        assert_eq!(
            observations
                .update(&p, "a", call("s", None), false, true, 104)
                .unwrap()
                .state,
            "ready"
        );
        let restart_epoch = observations.epoch();
        observations
            .update(
                &principal("other"),
                "b",
                call("unrelated", Some(Blocker::Quota)),
                false,
                false,
                106,
            )
            .unwrap();
        observations.lifecycle(&p, "a", call("s", None), false, Some(restart_epoch), 107);
        assert!(
            observations
                .update_fenced(
                    &p,
                    "a",
                    call("s", None),
                    (true, true),
                    Some(observations.epoch()),
                    108
                )
                .is_err()
        );
        assert!(
            !observations
                .0
                .lock()
                .unwrap()
                .values()
                .any(|row| row.principal.token_id == "one" && row.status(108).state == "ready")
        );
    }

    #[test]
    fn reports_are_bounded_and_read_only_expired_or_invalid_sessions_do_not_mutate() {
        let observations = Observations::default();
        let mut p = principal("one");
        p.read_only = true;
        assert!(
            observations
                .update(&p, "a", call("s", None), true, true, 100)
                .is_err()
        );
        p.read_only = false;
        assert!(
            observations
                .update(&p, "a", call("\n", None), true, true, 100)
                .is_err()
        );
        assert!(
            observations
                .update(&p, "a", call("s", None), true, true, p.expires_at)
                .is_err()
        );
        for n in 0..MAX_ROWS + 10 {
            observations
                .update(
                    &p,
                    "a",
                    call(&n.to_string(), None),
                    false,
                    false,
                    100 + n as i64,
                )
                .unwrap();
        }
        assert_eq!(observations.0.lock().unwrap().len(), MAX_TOKEN_ROWS);
    }

    #[test]
    fn churn_cannot_evict_another_token_or_forge_and_pre_session_diagnostics_cannot_verify() {
        let observations = Observations::default();
        let other = principal("other");
        start(&observations, &other, "a", "live", 100);
        observations
            .update(&other, "a", call("live", None), true, true, 100)
            .unwrap();
        let p = principal("churn");
        for n in 0..MAX_ROWS + 10 {
            observations
                .update(&p, "a", call(&n.to_string(), None), false, false, 101)
                .unwrap();
        }
        assert!(
            observations
                .0
                .lock()
                .unwrap()
                .values()
                .any(|row| row.principal.token_id == "other" && row.status(102).state == "ready")
        );
        assert!(
            observations
                .update(&p, "a", call(PENDING_SESSION, None), true, false, 102)
                .is_err()
        );
        let blocked = observations
            .update(
                &other,
                "a",
                call(PENDING_SESSION, Some(Blocker::Login)),
                false,
                false,
                103,
            )
            .unwrap();
        assert_eq!(blocked.state, "blocked");
        assert!(
            !observations
                .0
                .lock()
                .unwrap()
                .values()
                .any(|row| row.principal.token_id == "other" && row.status(104).state == "ready")
        );
        start(&observations, &other, "a", "new", 105);
        observations
            .update(&other, "a", call("new", None), true, true, 105)
            .unwrap();
        assert!(!observations.0.lock().unwrap().values().any(|row| row.principal.token_id == "other" && row.call.session == PENDING_SESSION));
        let mut foreign = other.clone();
        foreign.owner.identity.host = "forge.example.com".into();
        assert!(!same_repository(&other, &foreign));
        foreign.owner.identity.host = other.owner.identity.host.clone();
        foreign.owner.identity.provider = "gitlab".into();
        assert!(!same_repository(&other, &foreign));
    }

    #[test]
    fn diagnostic_classification_preserves_uncertainty_without_output() {
        for (text, expected) in [
            ("Not logged in", Blocker::Login),
            (r#"{"loggedIn":false}"#, Blocker::Login),
            ("invalid_grant", Blocker::Login),
            ("plugin untrusted", Blocker::Trust),
            ("MCP needs approval", Blocker::Trust),
            ("You've hit your usage limit", Blocker::Quota),
            ("429 rate_limit", Blocker::Quota),
            ("You've hit your limit", Blocker::Quota),
            ("UNSUPPORTED_CLIENT", Blocker::Unsupported),
            ("Connected successfully", Blocker::Unknown),
            ("file permission denied", Blocker::Unknown),
            ("Bearer synthetic-secret", Blocker::Unknown),
        ] {
            assert_eq!(Blocker::detect(text), expected);
        }
    }

    #[test]
    fn invalidation_fences_in_flight_proof_and_end_requires_native_restart() {
        let observations = Observations::default();
        let p = principal("one");
        start(&observations, &p, "a", "s", 100);
        let before = observations.epoch();
        observations
            .update(&p, "a", call("s", None), true, true, 100)
            .unwrap();
        observations
            .update(&p, "a", call("s", Some(Blocker::Trust)), false, false, 101)
            .unwrap();
        assert!(
            observations
                .update_fenced(&p, "a", call("s", None), (false, true), Some(before), 102)
                .is_err()
        );
        assert!(
            observations
                .update_fenced(&p, "a", call("s", None), (true, false), Some(before), 102)
                .is_err()
        );
        let current = observations.epoch();
        observations
            .update_fenced(&p, "a", call("s", None), (true, true), Some(current), 103)
            .unwrap();
        observations.lifecycle(&p, "a", call("s", None), true, None, 104);
        // A start whose request began before the newer end cannot reopen it.
        observations.lifecycle(&p, "a", call("s", None), false, Some(current), 104);
        assert!(
            observations
                .update_fenced(&p, "a", call("s", None), (true, true), Some(current), 104)
                .is_err()
        );
        assert!(
            observations
                .update_fenced(
                    &p,
                    "a",
                    call("s", None),
                    (true, true),
                    Some(observations.epoch()),
                    104
                )
                .is_err()
        );
        start(&observations, &p, "a", "s", 105);
        assert!(
            observations
                .update_fenced(&p, "a", call("s", None), (true, true), Some(current), 105)
                .is_err()
        );
        assert_eq!(
            observations
                .update_fenced(
                    &p,
                    "a",
                    call("s", None),
                    (true, true),
                    Some(observations.epoch()),
                    106
                )
                .unwrap()
                .state,
            "ready"
        );
    }

    #[test]
    fn repository_and_global_capacity_reject_new_rows_without_eviction() {
        let observations = Observations::default();
        for repo in 0..MAX_ROWS / MAX_REPO_ROWS {
            for token in 0..MAX_REPO_ROWS / MAX_TOKEN_ROWS {
                let mut p = principal(&format!("{repo}-{token}"));
                p.repository = format!("acme/repo-{repo}");
                for session in 0..MAX_TOKEN_ROWS {
                    observations
                        .update(&p, "a", call(&session.to_string(), None), false, false, 100)
                        .unwrap();
                }
            }
            let before: Vec<_> = observations.0.lock().unwrap().keys().cloned().collect();
            let mut extra = principal(&format!("extra-{repo}"));
            extra.repository = format!("acme/repo-{repo}");
            assert!(matches!(
                observations.update(&extra, "a", call("new", None), false, false, 101),
                Err(HubError::Busy)
            ));
            assert_eq!(
                observations
                    .0
                    .lock()
                    .unwrap()
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>(),
                before
            );
        }
        let before: Vec<_> = observations.0.lock().unwrap().keys().cloned().collect();
        let mut extra = principal("extra-global");
        extra.repository = "acme/another".into();
        assert!(matches!(
            observations.update(&extra, "a", call("new", None), false, false, 101),
            Err(HubError::Busy)
        ));
        assert_eq!(
            observations
                .0
                .lock()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>(),
            before
        );
        // Even an unseen end at capacity cannot be resurrected after cache expiry:
        // every newly admitted row is closed until an authenticated native start.
        observations.lifecycle(&extra, "a", call("ended", None), true, None, 101);
        assert!(
            observations
                .update_fenced(
                    &extra,
                    "a",
                    call("ended", None),
                    (true, true),
                    Some(observations.epoch()),
                    100 + TTL + 1
                )
                .is_err()
        );
        start(&observations, &extra, "a", "ended", 100 + TTL + 2);
        assert_eq!(
            observations
                .update_fenced(
                    &extra,
                    "a",
                    call("ended", None),
                    (true, true),
                    Some(observations.epoch()),
                    100 + TTL + 3
                )
                .unwrap()
                .state,
            "ready"
        );
    }
}
