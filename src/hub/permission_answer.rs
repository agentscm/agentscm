//! Allowing or denying one permission prompt of a waiting session from the Hub, for the person
//! who owns that session's agent token.
//!
//! The session's hook registers the prompt it is showing (the tool, one masked display line and
//! a salted hash of the tool input, all computed on the machine) and long-polls for a decision.
//! The person's own page lists the requests and answers one. The hook accepts only an exact
//! decision for its own request id and hash and prints a fixed reply, so nothing from the Hub
//! reaches the model.
//!
//! In memory only, like the waits beside it ([`super::session_waiting`]): bounded, lost on a Hub
//! restart (a hook then reads an unknown request and leaves the prompt to the session), and never
//! written to either store, a log, a notification or a status read. A request is live only while
//! the permission wait it was made for is still the session's current wait, so a prompt answered
//! in the session, or replaced by anything else, ends it.
//!
//! A request starts `unconfirmed` and becomes `pending` only when the Hub also hears the host's
//! `permission_prompt` notification for that session. A host that runs without a person at a
//! dialog (a headless run) raises the permission request but never shows the prompt, so it never
//! sends that notification and is never offered for answering.
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::Duration;

use axum::Router;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Json, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Notify, Semaphore};
use tokio::time::Instant;

use super::advice::invisible;
use super::http::{ApiError, Authenticated, JsonBody, blocking};
use super::login::HumanSession;
use super::session_waiting::{GRACE_SECONDS, Waiting, wait_key};
use super::store::{Principal, unix_now, validate_repository};
use super::{HubError, HubState};

/// A person may be away for hours; the hook's own bound is a little shorter than this.
const TTL_SECONDS: i64 = 12 * 60 * 60;
/// A request whose hook stopped polling is not answerable any more.
const HOOK_GONE_SECONDS: i64 = 60;
/// A finished request stays readable so a late answer can say what happened.
const RETAIN_SECONDS: i64 = 120;
/// Live requests one agent token may hold; registering another ends the oldest.
const MAX_PER_TOKEN: usize = 8;
const MAX_LIVE: usize = 256;
/// Long polls held open at once; past it a poll answers at once.
const MAX_POLLERS: usize = 256;
const MAX_WAIT_SECONDS: u64 = 25;
/// What the host's notification may lead the hook's own request by.
const CONFIRM_SLACK_SECONDS: i64 = 5;
const CONFIRM_KEEP_SECONDS: i64 = 60;
const MAX_CONFIRMATIONS: usize = 1024;
const MAX_SESSION_BYTES: usize = 1024;
const MAX_AGENT_ID_BYTES: usize = 128;
const MAX_TOOL_BYTES: usize = 128;
/// Characters of the display line kept; the hook cuts at the same bound.
pub const MAX_DISPLAY_CHARS: usize = 4000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Decision {
    Allow,
    Deny,
}

impl Decision {
    fn code(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    fn parse(code: &str) -> Option<Self> {
        match code {
            "allow" => Some(Self::Allow),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ended {
    /// Its permission wait is gone or replaced: it was answered in the session, or the session
    /// moved on.
    SessionMovedOn,
    Superseded,
    HookGone,
    Expired,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// Registered, but the host has not yet said it shows the prompt.
    Unconfirmed,
    Pending,
    Answered(Decision),
    Ended(Ended),
}

// No `Debug`: a row holds the person's tool request.
struct Pending {
    token: String,
    native: String,
    asker: Option<String>,
    owner: String,
    repository_id: Option<i64>,
    item_key: String,
    tool: String,
    display: String,
    complete: bool,
    content_hidden: bool,
    input_hash: String,
    created: i64,
    last_poll: i64,
    /// When it was answered or ended.
    settled: i64,
    state: State,
}

impl Pending {
    fn open(&self) -> bool {
        matches!(self.state, State::Unconfirmed | State::Pending)
    }

    fn end(&mut self, why: Ended, now: i64) {
        self.state = State::Ended(why);
        self.settled = now;
    }

    /// The start of the wait this request was made for, while that is still the session's wait.
    fn attached(&self, waiting: &Waiting, now: i64) -> Option<i64> {
        waiting
            .permission_wait(&self.token, &self.native, now)
            .filter(|(since, asker)| *since <= self.created && *asker == self.asker)
            .map(|(since, _)| since)
    }
}

#[derive(Default)]
struct Inner {
    rows: BTreeMap<String, Pending>,
    /// When the host last said it shows a permission prompt, per token and session.
    prompts: BTreeMap<(String, String), i64>,
}

impl Inner {
    /// Ends what has expired, lost its hook or lost its wait, and drops what was kept long enough.
    fn settle(&mut self, waiting: &Waiting, now: i64) {
        for row in self.rows.values_mut().filter(|row| row.open()) {
            if now - row.created >= TTL_SECONDS {
                row.end(Ended::Expired, now);
            } else if now - row.last_poll >= HOOK_GONE_SECONDS {
                row.end(Ended::HookGone, now);
            } else if row.attached(waiting, now).is_none() {
                row.end(Ended::SessionMovedOn, now);
            }
        }
        self.rows
            .retain(|_, row| row.open() || now - row.settled < RETAIN_SECONDS);
    }
}

/// Why a call was refused; each maps to one HTTP answer.
#[derive(Debug)]
pub(super) enum Refusal {
    Invalid(String),
    /// No current permission wait of that session, asked by that subagent.
    NotWaiting,
    Busy,
    NotFound,
    NotYours,
    AlreadyAnswered,
    AnsweredInSession,
    Ended,
    Hub(HubError),
}

impl From<HubError> for Refusal {
    fn from(error: HubError) -> Self {
        Self::Hub(error)
    }
}

type Outcome<T> = std::result::Result<T, Refusal>;

fn conflict(reason: &str, message: &str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, "request_conflict")
        .with_message(message)
        .with_field("reason", json!(reason))
}

fn not_your_agent() -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, "not_your_agent")
        .with_message("That agent is not yours, or it is no longer connected")
}

impl From<Refusal> for ApiError {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::Invalid(message) => ApiError::invalid().with_message(message),
            Refusal::NotWaiting => conflict(
                "not_waiting",
                "That session is not waiting at a permission prompt",
            ),
            Refusal::Busy => ApiError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited")
                .with_message("Too many permission requests are open; try again shortly")
                .with_field("reason", json!("busy")),
            Refusal::NotFound => ApiError::not_found(),
            Refusal::NotYours => not_your_agent(),
            Refusal::AlreadyAnswered => conflict(
                "already_answered",
                "This request was already answered the other way",
            ),
            Refusal::AnsweredInSession => conflict(
                "answered_in_session",
                "The session moved on; it was answered there or the prompt closed.",
            ),
            Refusal::Ended => conflict("ended", "This request is over; its prompt is closed."),
            Refusal::Hub(error) => error.into(),
        }
    }
}

/// What a hook registers: the facts of one prompt, all made on the machine.
pub(super) struct Registration {
    pub session: String,
    pub agent_id: Option<String>,
    pub tool: String,
    pub display: String,
    pub complete: bool,
    pub content_hidden: bool,
    pub input_hash: String,
}

/// What `poll` reports to the hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Poll {
    Unconfirmed,
    Pending,
    Answered(Decision),
    Ended,
}

impl Poll {
    fn parts(self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Unconfirmed => ("unconfirmed", None),
            Self::Pending => ("pending", None),
            Self::Answered(decision) => ("answered", Some(decision.code())),
            Self::Ended => ("ended", None),
        }
    }
}

/// One request as its person's page sees it. Never the hash, token or session.
#[derive(Serialize)]
pub(super) struct Listed {
    item_id: String,
    request_id: String,
    tool: String,
    display: String,
    complete: bool,
    content_hidden: bool,
    subagent: bool,
    since: i64,
    state: &'static str,
    decision: Option<&'static str>,
}

/// The pending permission requests of this Hub process.
pub struct Requests {
    inner: Mutex<Inner>,
    changed: Notify,
    polls: Semaphore,
}

impl Default for Requests {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            changed: Notify::new(),
            polls: Semaphore::new(MAX_POLLERS),
        }
    }
}

/// Third-party text for the person's page: control and invisible characters are dropped, except
/// that line breaks stay and a tab reads as a space. Cut at [`MAX_DISPLAY_CHARS`]; the flag says
/// whether it was.
fn sanitize_display(text: &str) -> (String, bool) {
    let mut kept: String = text
        .chars()
        .filter(|c| !invisible(*c))
        .filter_map(|c| match c {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            c if c.is_control() => None,
            c => Some(c),
        })
        .collect();
    let cut = kept.chars().count() > MAX_DISPLAY_CHARS;
    if cut {
        kept = kept.chars().take(MAX_DISPLAY_CHARS).collect();
    }
    (kept.trim().to_owned(), cut)
}

fn plain_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

fn random_id() -> Outcome<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|_| HubError::Unavailable("random source unavailable".into()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl Requests {
    fn lock(&self) -> Outcome<std::sync::MutexGuard<'_, Inner>> {
        Ok(self.inner.lock().map_err(|_| HubError::Busy)?)
    }

    /// Records a prompt the hook is showing. Needs the session's current permission wait, asked
    /// by the same subagent. A repeat for the same prompt returns the same request; a different
    /// prompt of the session ends the older request.
    pub(super) fn register(
        &self,
        waiting: &Waiting,
        principal: &Principal,
        registration: Registration,
        now: i64,
    ) -> Outcome<(String, Poll)> {
        if principal.read_only || now >= principal.expires_at {
            return Err(HubError::Unauthorized.into());
        }
        let Registration {
            session,
            agent_id,
            tool,
            display,
            complete,
            content_hidden,
            input_hash,
        } = registration;
        let bad = |what: &str| Refusal::Invalid(format!("{what} is not acceptable"));
        if !plain_text(&session, MAX_SESSION_BYTES) {
            return Err(bad("session"));
        }
        if agent_id
            .as_deref()
            .is_some_and(|id| !plain_text(id, MAX_AGENT_ID_BYTES))
        {
            return Err(bad("agent_id"));
        }
        if tool.is_empty()
            || tool.len() > MAX_TOOL_BYTES
            || !tool
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'))
        {
            return Err(bad("tool"));
        }
        if input_hash.len() != 64
            || !input_hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(bad("input_hash"));
        }
        let (display, cut) = sanitize_display(&display);
        if display.is_empty() {
            return Err(bad("display"));
        }
        let mut inner = self.lock()?;
        inner.settle(waiting, now);
        match waiting.permission_wait(&principal.token_id, &session, now) {
            Some((_, asker)) if asker == agent_id => {}
            _ => return Err(Refusal::NotWaiting),
        }
        let same = |row: &Pending| {
            row.token == principal.token_id && row.native == session && row.asker == agent_id
        };
        if let Some((id, row)) = inner
            .rows
            .iter_mut()
            .find(|(_, row)| row.open() && same(row) && row.input_hash == input_hash)
        {
            row.last_poll = now;
            let state = if row.state == State::Pending {
                Poll::Pending
            } else {
                Poll::Unconfirmed
            };
            return Ok((id.clone(), state));
        }
        for row in inner
            .rows
            .values_mut()
            .filter(|row| row.open() && row.token == principal.token_id && row.native == session)
        {
            row.end(Ended::Superseded, now);
        }
        let open: Vec<(i64, String)> = inner
            .rows
            .iter()
            .filter(|(_, row)| row.open() && row.token == principal.token_id)
            .map(|(id, row)| (row.created, id.clone()))
            .collect();
        if open.len() >= MAX_PER_TOKEN
            && let Some((_, oldest)) = open.into_iter().min()
            && let Some(row) = inner.rows.get_mut(&oldest)
        {
            row.end(Ended::Superseded, now);
        }
        if inner.rows.len() >= MAX_LIVE {
            // Finished rows only wait to answer a late click; they never crowd out a live one.
            inner.rows.retain(|_, row| row.open());
        }
        if inner.rows.len() >= MAX_LIVE {
            return Err(Refusal::Busy);
        }
        let confirmed = inner
            .prompts
            .get(&(principal.token_id.clone(), session.clone()))
            .is_some_and(|at| *at + CONFIRM_SLACK_SECONDS >= now);
        let id = random_id()?;
        let state = if confirmed {
            State::Pending
        } else {
            State::Unconfirmed
        };
        inner.rows.insert(
            id.clone(),
            Pending {
                token: principal.token_id.clone(),
                item_key: wait_key(&principal.token_id, &session),
                native: session,
                asker: agent_id,
                owner: principal.owner.id.clone(),
                repository_id: principal.repository_id,
                tool,
                display,
                complete: complete && !cut,
                content_hidden,
                input_hash,
                created: now,
                last_poll: now,
                settled: now,
                state,
            },
        );
        drop(inner);
        self.changed.notify_waiters();
        Ok((id, if confirmed { Poll::Pending } else { Poll::Unconfirmed }))
    }

    /// The host showed (or is about to show) a permission prompt for `native`: its unconfirmed
    /// request becomes pending, and a request registered within a few seconds after it is
    /// confirmed on arrival.
    pub(super) fn confirm(&self, token: &str, native: &str, now: i64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let key = (token.to_owned(), native.to_owned());
        inner
            .prompts
            .retain(|_, at| now - *at < CONFIRM_KEEP_SECONDS);
        if inner.prompts.len() >= MAX_CONFIRMATIONS && !inner.prompts.contains_key(&key) {
            return;
        }
        inner.prompts.insert(key, now);
        let mut changed = false;
        for row in inner.rows.values_mut() {
            if row.state == State::Unconfirmed && row.token == token && row.native == native {
                row.state = State::Pending;
                changed = true;
            }
        }
        drop(inner);
        if changed {
            self.changed.notify_waiters();
        }
    }

    /// The state of a request for the hook that made it; `None` for an unknown id, another
    /// token's request or a different hash, which look alike.
    pub(super) fn check(
        &self,
        waiting: &Waiting,
        token: &str,
        id: &str,
        hash: &str,
        now: i64,
    ) -> Option<Poll> {
        let mut inner = self.inner.lock().ok()?;
        inner.settle(waiting, now);
        let row = inner.rows.get_mut(id)?;
        if row.token != token || row.input_hash != hash {
            return None;
        }
        if row.open() {
            row.last_poll = now;
        }
        Some(match row.state {
            State::Unconfirmed => Poll::Unconfirmed,
            State::Pending => Poll::Pending,
            State::Answered(decision) => Poll::Answered(decision),
            State::Ended(_) => Poll::Ended,
        })
    }

    /// [`Self::check`], held for up to `wait_seconds` until the state changes. A change wakes
    /// the poll at once; a gone wait is noticed within a second.
    pub(super) async fn poll(
        &self,
        waiting: &Waiting,
        token: &str,
        id: &str,
        hash: &str,
        wait_seconds: u64,
    ) -> Option<Poll> {
        let permit = self.polls.try_acquire().ok();
        let wait = if permit.is_some() { wait_seconds } else { 0 };
        let deadline = Instant::now() + Duration::from_secs(wait);
        let mut first = None;
        loop {
            // Register before reading so a change between read and wait is not missed.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = self.check(waiting, token, id, hash, unix_now())?;
            let open = matches!(state, Poll::Unconfirmed | Poll::Pending);
            if !open || first.is_some_and(|first| first != state) || Instant::now() >= deadline {
                return Some(state);
            }
            first = Some(state);
            tokio::select! {
                _ = &mut notified => {},
                _ = tokio::time::sleep_until(deadline.min(Instant::now() + Duration::from_secs(1))) => {},
            }
        }
    }

    /// The agent token behind one of `owner`'s requests, for the caller to check it is still
    /// active before answering.
    pub(super) fn token_for(
        &self,
        owner: &str,
        repository_id: Option<i64>,
        id: &str,
    ) -> Outcome<String> {
        let inner = self.lock()?;
        let row = inner.rows.get(id).ok_or(Refusal::NotFound)?;
        if row.owner != owner || row.repository_id != repository_id {
            return Err(Refusal::NotYours);
        }
        Ok(row.token.clone())
    }

    /// The pending and answered requests of `owner` in one repository whose wait has lasted the
    /// grace, oldest first. Unconfirmed requests are never listed.
    pub(super) fn list(
        &self,
        waiting: &Waiting,
        owner: &str,
        repository_id: Option<i64>,
        now: i64,
    ) -> Outcome<Vec<Listed>> {
        let mut inner = self.lock()?;
        inner.settle(waiting, now);
        let mut out = Vec::new();
        for (id, row) in &inner.rows {
            if row.owner != owner || row.repository_id != repository_id {
                continue;
            }
            let (state, decision) = match row.state {
                State::Pending => ("pending", None),
                State::Answered(decision) => ("answered", Some(decision.code())),
                State::Unconfirmed | State::Ended(_) => continue,
            };
            let Some(since) = row.attached(waiting, now) else {
                continue;
            };
            if now - since < GRACE_SECONDS {
                continue;
            }
            out.push(Listed {
                item_id: format!("waiting:permission:{}", row.item_key),
                request_id: id.clone(),
                tool: row.tool.clone(),
                display: row.display.clone(),
                complete: row.complete,
                content_hidden: row.content_hidden,
                subagent: row.asker.is_some(),
                since,
                state,
                decision,
            });
        }
        out.sort_by(|a, b| (a.since, &a.request_id).cmp(&(b.since, &b.request_id)));
        Ok(out)
    }

    /// Whether the person can answer the wait with this item key from the Hub now.
    #[allow(dead_code)] // The Needs-you item reads it once `waiting_actions` is wired to it.
    pub(super) fn answerable(&self, waiting: &Waiting, key: &str, now: i64) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        inner.settle(waiting, now);
        inner
            .rows
            .values()
            .any(|row| row.item_key == key && row.state == State::Pending)
    }

    /// Records the person's decision for a pending request: the first decision wins, the same
    /// decision again is accepted, and Allow needs the whole request to have been shown.
    pub(super) fn answer(
        &self,
        waiting: &Waiting,
        owner: &str,
        repository_id: Option<i64>,
        id: &str,
        decision: Decision,
        now: i64,
    ) -> Outcome<Decision> {
        let mut inner = self.lock()?;
        inner.settle(waiting, now);
        let row = inner.rows.get_mut(id).ok_or(Refusal::NotFound)?;
        if row.owner != owner || row.repository_id != repository_id {
            return Err(Refusal::NotYours);
        }
        match row.state {
            State::Unconfirmed => Err(Refusal::NotFound),
            State::Pending => {
                if decision == Decision::Allow && !row.complete {
                    return Err(Refusal::Invalid(
                        "Allow is offered only when the whole request is shown; deny it, or answer in the session"
                            .into(),
                    ));
                }
                row.state = State::Answered(decision);
                row.settled = now;
                drop(inner);
                self.changed.notify_waiters();
                Ok(decision)
            }
            State::Answered(done) if done == decision => Ok(done),
            State::Answered(_) => Err(Refusal::AlreadyAnswered),
            State::Ended(Ended::SessionMovedOn) => Err(Refusal::AnsweredInSession),
            State::Ended(_) => Err(Refusal::Ended),
        }
    }
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route("/v1/hub/permission/register", post(register))
        .route("/v1/hub/permission/poll", post(poll))
        .route("/v1/hub/session/permissions", get(list))
        .route("/v1/hub/session/permissions/answer", post(answer))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HookQuery {
    repo: Option<String>,
}

/// The hook's token must be for the repository it names, as on the Hub's hook route.
fn same_repo(
    query: std::result::Result<Query<HookQuery>, QueryRejection>,
    principal: &Principal,
) -> std::result::Result<(), ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    if query.repo.is_some_and(|repo| repo != principal.repository) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "wrong_repository"));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RegisterBody {
    session: String,
    #[serde(default)]
    agent_id: Option<String>,
    tool: String,
    display: String,
    complete: bool,
    #[serde(default)]
    content_hidden: bool,
    input_hash: String,
}

#[derive(Serialize)]
struct Registered {
    request_id: String,
    input_hash: String,
    state: &'static str,
}

async fn register(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    query: std::result::Result<Query<HookQuery>, QueryRejection>,
    JsonBody(body): JsonBody<RegisterBody>,
) -> std::result::Result<Json<Registered>, ApiError> {
    same_repo(query, &principal)?;
    let input_hash = body.input_hash.clone();
    let (request_id, polled) = state.onboarding.permissions().register(
        state.onboarding.waiting(),
        &principal,
        Registration {
            session: body.session,
            agent_id: body.agent_id,
            tool: body.tool,
            display: body.display,
            complete: body.complete,
            content_hidden: body.content_hidden,
            input_hash: body.input_hash,
        },
        unix_now(),
    )?;
    Ok(Json(Registered {
        request_id,
        input_hash,
        state: polled.parts().0,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PollBody {
    request_id: String,
    input_hash: String,
    #[serde(default)]
    wait_seconds: u64,
}

#[derive(Serialize)]
struct Polled {
    request_id: String,
    input_hash: String,
    state: &'static str,
    decision: Option<&'static str>,
}

async fn poll(
    State(state): State<HubState>,
    Authenticated(principal): Authenticated,
    query: std::result::Result<Query<HookQuery>, QueryRejection>,
    JsonBody(body): JsonBody<PollBody>,
) -> std::result::Result<Json<Polled>, ApiError> {
    same_repo(query, &principal)?;
    if body.wait_seconds > MAX_WAIT_SECONDS {
        return Err(ApiError::invalid().with_message("wait_seconds must be 0..=25"));
    }
    let polled = state
        .onboarding
        .permissions()
        .poll(
            state.onboarding.waiting(),
            &principal.token_id,
            &body.request_id,
            &body.input_hash,
            body.wait_seconds,
        )
        .await
        .ok_or_else(ApiError::not_found)?;
    let (name, decision) = polled.parts();
    Ok(Json(Polled {
        request_id: body.request_id,
        input_hash: body.input_hash,
        state: name,
        decision,
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryQuery {
    repository: String,
}

#[derive(Serialize)]
struct Page {
    requests: Vec<Listed>,
}

async fn list(
    State(state): State<HubState>,
    human: HumanSession,
    query: std::result::Result<Query<RepositoryQuery>, QueryRejection>,
) -> std::result::Result<Json<Page>, ApiError> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let user = human.web_user()?.clone();
    validate_repository(&query.repository)?;
    let repo =
        super::login::devices::mint_check(&state, &user, &query.repository, "permissions").await?;
    let requests = state.onboarding.permissions().list(
        state.onboarding.waiting(),
        &user.id,
        Some(repo.repository_id),
        unix_now(),
    )?;
    Ok(Json(Page { requests }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerBody {
    repository: String,
    request_id: String,
    decision: String,
}

#[derive(Serialize)]
struct Answered {
    state: &'static str,
    decision: &'static str,
}

/// A signed-in person allows or denies one prompt of their own agent. The web sign-in and its
/// CSRF token are checked by [`HumanSession`]; the agent token must still be an active token of
/// the same person.
async fn answer(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<AnswerBody>,
) -> std::result::Result<Json<Answered>, ApiError> {
    let user = human.web_user()?.clone();
    validate_repository(&body.repository)?;
    let decision = Decision::parse(&body.decision)
        .ok_or_else(|| ApiError::invalid().with_message("decision must be allow or deny"))?;
    let repo =
        super::login::devices::mint_check(&state, &user, &body.repository, "permissions").await?;
    let requests = state.onboarding.permissions();
    let repository_id = Some(repo.repository_id);
    let token = requests.token_for(&user.id, repository_id, &body.request_id)?;
    let store = state.store.clone();
    let owner = user.id.clone();
    blocking(&state, move || {
        if super::coordination_http::owns_agent(&*store, &owner, &token, unix_now())? {
            Ok(())
        } else {
            Err(HubError::Unauthorized)
        }
    })
    .await
    .map_err(|error| match error {
        HubError::Unauthorized => not_your_agent(),
        other => other.into(),
    })?;
    let done = requests.answer(
        state.onboarding.waiting(),
        &user.id,
        repository_id,
        &body.request_id,
        decision,
        unix_now(),
    )?;
    Ok(Json(Answered {
        state: "answered",
        decision: done.code(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::session_waiting::WaitReason;
    use crate::hub::store::{ForgeIdentity, User};

    const HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const OTHER_HASH: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    fn principal(token: &str, owner: &str, repository: &str) -> Principal {
        Principal {
            token_id: token.into(),
            name: format!("{token}-laptop"),
            repository: repository.into(),
            repository_id: Some(7),
            expires_at: i64::MAX,
            read_only: false,
            owner: User {
                id: owner.into(),
                identity: ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                login: owner.into(),
                name: None,
                disabled: false,
            },
        }
    }

    fn registration(session: &str, agent_id: Option<&str>, hash: &str) -> Registration {
        Registration {
            session: session.into(),
            agent_id: agent_id.map(str::to_owned),
            tool: "Bash".into(),
            display: "git push origin main".into(),
            complete: true,
            content_hidden: false,
            input_hash: hash.into(),
        }
    }

    fn wait(
        waiting: &Waiting,
        who: &Principal,
        native: &str,
        reason: WaitReason,
        asker: Option<&str>,
        now: i64,
    ) {
        waiting
            .record(who, native, native, "claude", reason, asker, now)
            .unwrap();
    }

    /// A confirmed pending request of `mine` on session `s`, waiting since 1_000.
    fn pending(requests: &Requests, waiting: &Waiting, mine: &Principal) -> String {
        wait(waiting, mine, "s", WaitReason::Permission, None, 1_000);
        requests.confirm(&mine.token_id, "s", 1_001);
        let (id, state) = requests
            .register(waiting, mine, registration("s", None, HASH), 1_002)
            .unwrap();
        assert_eq!(state, Poll::Pending);
        id
    }

    #[test]
    fn registering_needs_the_same_permission_wait_and_a_usable_token() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let register = |p: &Principal, session, agent: Option<&str>| {
            requests
                .register(&waiting, p, registration(session, agent, HASH), 1_010)
                .map(|(_, state)| state)
        };
        assert!(matches!(
            register(&mine, "s", None),
            Err(Refusal::NotWaiting)
        ));
        wait(&waiting, &mine, "s", WaitReason::Idle, None, 1_000);
        assert!(matches!(
            register(&mine, "s", None),
            Err(Refusal::NotWaiting)
        ));
        wait(&waiting, &mine, "s", WaitReason::Permission, Some("sub"), 1_001);
        assert!(matches!(
            register(&mine, "s", None),
            Err(Refusal::NotWaiting)
        ));
        assert!(matches!(
            register(&mine, "s", Some("other")),
            Err(Refusal::NotWaiting)
        ));
        assert!(matches!(
            register(&mine, "s", Some("sub")),
            Ok(Poll::Unconfirmed)
        ));
        // Another token has no wait under that session name.
        let theirs = principal("tok_b", "usr_them", "acme/widgets");
        assert!(matches!(
            register(&theirs, "s", Some("sub")),
            Err(Refusal::NotWaiting)
        ));
        let mut reader = mine.clone();
        reader.read_only = true;
        assert!(matches!(
            register(&reader, "s", Some("sub")),
            Err(Refusal::Hub(HubError::Unauthorized))
        ));
    }

    #[test]
    fn registering_again_returns_the_request_and_a_new_prompt_supersedes_it() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_000);
        let first = requests
            .register(&waiting, &mine, registration("s", None, HASH), 1_001)
            .unwrap()
            .0;
        let again = requests
            .register(&waiting, &mine, registration("s", None, HASH), 1_002)
            .unwrap()
            .0;
        assert_eq!(first, again);
        let second = requests
            .register(&waiting, &mine, registration("s", None, OTHER_HASH), 1_003)
            .unwrap()
            .0;
        assert_ne!(first, second);
        assert_eq!(
            requests.check(&waiting, "tok_a", &first, HASH, 1_004),
            Some(Poll::Ended)
        );
        assert_eq!(
            requests.check(&waiting, "tok_a", &second, OTHER_HASH, 1_004),
            Some(Poll::Unconfirmed)
        );
    }

    #[test]
    fn a_token_and_the_whole_hub_hold_a_bounded_number_of_live_requests() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let mut ids = Vec::new();
        for n in 0..=MAX_PER_TOKEN {
            let session = format!("s{n}");
            wait(&waiting, &mine, &session, WaitReason::Permission, None, 1_000);
            let at = 1_001 + i64::try_from(n).unwrap();
            ids.push(
                requests
                    .register(&waiting, &mine, registration(&session, None, HASH), at)
                    .unwrap()
                    .0,
            );
        }
        // The oldest gave way to the ninth.
        let at = 1_010;
        assert_eq!(
            requests.check(&waiting, "tok_a", &ids[0], HASH, at),
            Some(Poll::Ended)
        );
        assert_eq!(
            requests.check(&waiting, "tok_a", &ids[MAX_PER_TOKEN], HASH, at),
            Some(Poll::Unconfirmed)
        );
        let mut refused = false;
        'fill: for t in 0..(MAX_LIVE / MAX_PER_TOKEN + 2) {
            let who = principal(&format!("tok_{t}"), "usr_x", "acme/widgets");
            for n in 0..MAX_PER_TOKEN {
                let session = format!("c{n}");
                wait(&waiting, &who, &session, WaitReason::Permission, None, 1_000);
                let outcome =
                    requests.register(&waiting, &who, registration(&session, None, HASH), 1_200);
                if matches!(outcome, Err(Refusal::Busy)) {
                    refused = true;
                    break 'fill;
                }
                outcome.unwrap();
            }
        }
        assert!(refused, "the hub-wide bound refused a request");
    }

    #[test]
    fn a_request_ends_when_its_hook_goes_quiet_or_at_the_limit() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_000);
        let id = requests
            .register(&waiting, &mine, registration("s", None, HASH), 1_000)
            .unwrap()
            .0;
        let quiet = 1_000 + HOOK_GONE_SECONDS;
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, quiet),
            Some(Poll::Ended)
        );
        // Kept polling, it lives until the limit.
        let id = requests
            .register(&waiting, &mine, registration("s", None, OTHER_HASH), quiet)
            .unwrap()
            .0;
        let mut now = quiet;
        while now - quiet < TTL_SECONDS - 30 {
            now += 30;
            wait(&waiting, &mine, "s", WaitReason::Permission, None, now);
            assert_eq!(
                requests.check(&waiting, "tok_a", &id, OTHER_HASH, now),
                Some(Poll::Unconfirmed)
            );
        }
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, OTHER_HASH, quiet + TTL_SECONDS),
            Some(Poll::Ended)
        );
    }

    #[test]
    fn only_the_registering_token_with_the_right_id_and_hash_can_poll() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let id = pending(&requests, &waiting, &mine);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_003),
            Some(Poll::Pending)
        );
        assert_eq!(requests.check(&waiting, "tok_b", &id, HASH, 1_003), None);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, OTHER_HASH, 1_003),
            None
        );
        assert_eq!(
            requests.check(&waiting, "tok_a", "nope", HASH, 1_003),
            None
        );
    }

    #[tokio::test]
    async fn a_held_poll_returns_when_the_person_answers() {
        let requests = std::sync::Arc::new(Requests::default());
        let waiting = std::sync::Arc::new(Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let now = unix_now();
        wait(&waiting, &mine, "s", WaitReason::Permission, None, now);
        requests.confirm("tok_a", "s", now);
        let id = requests
            .register(&waiting, &mine, registration("s", None, HASH), now)
            .unwrap()
            .0;
        let answering = {
            let (requests, waiting, id) = (requests.clone(), waiting.clone(), id.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                requests
                    .answer(
                        &waiting,
                        "usr_me",
                        Some(7),
                        &id,
                        Decision::Allow,
                        unix_now(),
                    )
                    .unwrap()
            })
        };
        let started = Instant::now();
        let polled = requests.poll(&waiting, "tok_a", &id, HASH, 20).await;
        assert_eq!(polled, Some(Poll::Answered(Decision::Allow)));
        assert!(started.elapsed() < Duration::from_secs(5));
        answering.await.unwrap();
        // A lost reply can be read again.
        assert_eq!(
            requests.poll(&waiting, "tok_a", &id, HASH, 0).await,
            Some(Poll::Answered(Decision::Allow))
        );
        assert_eq!(requests.poll(&waiting, "tok_b", &id, HASH, 0).await, None);
    }

    #[test]
    fn a_request_ends_when_its_wait_is_cleared_replaced_or_asked_by_another_subagent() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let id = pending(&requests, &waiting, &mine);
        waiting.clear("tok_a", "s", None, true);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_003),
            Some(Poll::Ended)
        );
        let id = pending(&requests, &waiting, &mine);
        wait(&waiting, &mine, "s", WaitReason::Idle, None, 1_050);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_051),
            Some(Poll::Ended)
        );
        // A permission wait asked by a subagent is a different wait.
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_060);
        let id = requests
            .register(&waiting, &mine, registration("s", None, OTHER_HASH), 1_061)
            .unwrap()
            .0;
        wait(&waiting, &mine, "s", WaitReason::Permission, Some("sub"), 1_062);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, OTHER_HASH, 1_063),
            Some(Poll::Ended)
        );
    }

    #[test]
    fn a_request_is_unconfirmed_until_the_host_shows_its_prompt() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_000);
        let id = requests
            .register(&waiting, &mine, registration("s", None, HASH), 1_001)
            .unwrap()
            .0;
        // Hidden from the person and not answerable while unconfirmed.
        assert!(
            requests
                .list(&waiting, "usr_me", Some(7), 1_000 + GRACE_SECONDS)
                .unwrap()
                .is_empty()
        );
        assert!(matches!(
            requests.answer(&waiting, "usr_me", Some(7), &id, Decision::Deny, 1_002),
            Err(Refusal::NotFound)
        ));
        // Another session's notification does not confirm it.
        requests.confirm("tok_a", "other", 1_003);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_004),
            Some(Poll::Unconfirmed)
        );
        requests.confirm("tok_a", "s", 1_005);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_006),
            Some(Poll::Pending)
        );
        // The notification may come first, within a few seconds before the request.
        wait(&waiting, &mine, "t", WaitReason::Permission, None, 2_000);
        requests.confirm("tok_a", "t", 2_001);
        let early = requests
            .register(&waiting, &mine, registration("t", None, HASH), 2_001 + CONFIRM_SLACK_SECONDS)
            .unwrap();
        assert_eq!(early.1, Poll::Pending);
        wait(&waiting, &mine, "u", WaitReason::Permission, None, 3_000);
        requests.confirm("tok_a", "u", 3_001);
        let late = requests
            .register(
                &waiting,
                &mine,
                registration("u", None, HASH),
                3_001 + CONFIRM_SLACK_SECONDS + 1,
            )
            .unwrap();
        assert_eq!(late.1, Poll::Unconfirmed);
    }

    #[test]
    fn only_the_owner_answers_once_and_allow_needs_the_whole_request() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let id = pending(&requests, &waiting, &mine);
        let answer = |owner, repository, decision| {
            requests.answer(&waiting, owner, repository, &id, decision, 1_010)
        };
        assert!(matches!(
            answer("usr_them", Some(7), Decision::Deny),
            Err(Refusal::NotYours)
        ));
        assert!(matches!(
            answer("usr_me", Some(8), Decision::Deny),
            Err(Refusal::NotYours)
        ));
        assert!(matches!(
            requests.answer(&waiting, "usr_me", Some(7), "nope", Decision::Deny, 1_010),
            Err(Refusal::NotFound)
        ));
        assert!(matches!(
            requests.token_for("usr_them", Some(7), &id),
            Err(Refusal::NotYours)
        ));
        assert_eq!(requests.token_for("usr_me", Some(7), &id).unwrap(), "tok_a");
        assert!(matches!(
            answer("usr_me", Some(7), Decision::Allow),
            Ok(Decision::Allow)
        ));
        assert!(matches!(
            answer("usr_me", Some(7), Decision::Allow),
            Ok(Decision::Allow)
        ));
        assert!(matches!(
            answer("usr_me", Some(7), Decision::Deny),
            Err(Refusal::AlreadyAnswered)
        ));
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_011),
            Some(Poll::Answered(Decision::Allow))
        );

        // A cut request can be denied but never allowed.
        let theirs = principal("tok_b", "usr_me", "acme/widgets");
        wait(&waiting, &theirs, "s", WaitReason::Permission, None, 1_000);
        requests.confirm("tok_b", "s", 1_001);
        let mut cut = registration("s", None, HASH);
        cut.complete = false;
        let cut_id = requests
            .register(&waiting, &theirs, cut, 1_002)
            .unwrap()
            .0;
        let on_cut = |decision| {
            requests.answer(&waiting, "usr_me", Some(7), &cut_id, decision, 1_010)
        };
        assert!(matches!(on_cut(Decision::Allow), Err(Refusal::Invalid(_))));
        assert!(matches!(on_cut(Decision::Deny), Ok(Decision::Deny)));
    }

    #[test]
    fn an_answer_after_the_prompt_closed_says_where_it_went() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let id = pending(&requests, &waiting, &mine);
        waiting.clear("tok_a", "s", None, true);
        assert!(matches!(
            requests.answer(&waiting, "usr_me", Some(7), &id, Decision::Allow, 1_010),
            Err(Refusal::AnsweredInSession)
        ));
        // Past the retention it is simply unknown.
        assert!(matches!(
            requests.answer(
                &waiting,
                "usr_me",
                Some(7),
                &id,
                Decision::Allow,
                1_010 + RETAIN_SECONDS
            ),
            Err(Refusal::NotFound)
        ));
        let id = pending(&requests, &waiting, &mine);
        // The hook went quiet: ended, but not because the session moved on.
        assert!(matches!(
            requests.answer(
                &waiting,
                "usr_me",
                Some(7),
                &id,
                Decision::Allow,
                1_002 + HOOK_GONE_SECONDS
            ),
            Err(Refusal::Ended)
        ));
    }

    #[test]
    fn the_list_shows_only_the_owners_confirmed_requests_in_one_repository_after_the_grace() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let id = pending(&requests, &waiting, &mine);
        let at = 1_000 + GRACE_SECONDS;
        // Inside the grace nothing shows.
        assert!(
            requests
                .list(&waiting, "usr_me", Some(7), at - 1)
                .unwrap()
                .is_empty()
        );
        // Another person, another repository: nothing.
        assert!(
            requests
                .list(&waiting, "usr_them", Some(7), at)
                .unwrap()
                .is_empty()
        );
        assert!(
            requests
                .list(&waiting, "usr_me", Some(8), at)
                .unwrap()
                .is_empty()
        );
        let shown = requests.list(&waiting, "usr_me", Some(7), at).unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(
            (shown[0].request_id.as_str(), shown[0].since, shown[0].state),
            (id.as_str(), 1_000, "pending")
        );
        assert_eq!(
            shown[0].item_id,
            format!("waiting:permission:{}", wait_key("tok_a", "s"))
        );
        let text = serde_json::to_string(&shown).unwrap();
        assert!(!text.contains(HASH) && !text.contains("tok_a") && !text.contains("\"s\""));
        assert!(requests.answerable(&waiting, &wait_key("tok_a", "s"), at));
        requests
            .answer(&waiting, "usr_me", Some(7), &id, Decision::Deny, at)
            .unwrap();
        let shown = requests.list(&waiting, "usr_me", Some(7), at).unwrap();
        assert_eq!((shown[0].state, shown[0].decision), ("answered", Some("deny")));
        assert!(!requests.answerable(&waiting, &wait_key("tok_a", "s"), at));
        // The hook stopped polling: stale, so hidden.
        let id = requests
            .register(&waiting, &mine, registration("s", None, OTHER_HASH), at)
            .unwrap()
            .0;
        requests.confirm("tok_a", "s", at);
        assert_eq!(requests.list(&waiting, "usr_me", Some(7), at).unwrap().len(), 2);
        assert!(
            requests
                .list(&waiting, "usr_me", Some(7), at + HOOK_GONE_SECONDS)
                .unwrap()
                .iter()
                .all(|listed| listed.request_id != id)
        );
    }

    #[test]
    fn display_text_loses_control_and_invisible_characters_but_keeps_lines_and_quotes() {
        let (text, cut) = sanitize_display("echo \"a\"\r\n\tls\u{1b}[31m \u{202e}x\u{200b}y\u{0}");
        assert_eq!(text, "echo \"a\"\n ls[31m xy");
        assert!(!cut);
        let (text, cut) = sanitize_display(&"x".repeat(MAX_DISPLAY_CHARS + 5));
        assert_eq!(text.chars().count(), MAX_DISPLAY_CHARS);
        assert!(cut);
        assert_eq!(sanitize_display(" \u{200b}\n ").0, "");
    }

    #[test]
    fn registration_checks_its_fields_and_flags_a_cut_display_as_incomplete() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_000);
        let attempt = |change: &dyn Fn(&mut Registration)| {
            let mut r = registration("s", None, HASH);
            change(&mut r);
            requests.register(&waiting, &mine, r, 1_001)
        };
        assert!(matches!(
            attempt(&|r| r.tool = "Ba sh".into()),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            attempt(&|r| r.tool = String::new()),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            attempt(&|r| r.input_hash = "ABC".into()),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            attempt(&|r| r.display = "\u{202e}\n ".into()),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            attempt(&|r| r.session = "s\n".into()),
            Err(Refusal::Invalid(_))
        ));
        let id = attempt(&|r| r.display = "y".repeat(MAX_DISPLAY_CHARS + 1))
            .unwrap()
            .0;
        requests.confirm("tok_a", "s", 1_002);
        assert!(matches!(
            requests.answer(&waiting, "usr_me", Some(7), &id, Decision::Allow, 1_003),
            Err(Refusal::Invalid(_))
        ));
    }

    #[test]
    fn the_item_key_matches_the_one_the_waiting_list_reports() {
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        wait(&waiting, &mine, "s1", WaitReason::Permission, None, 1_000);
        let shown = waiting.owned(&mine, 1_000 + GRACE_SECONDS).unwrap();
        assert_eq!(shown[0].key, wait_key("tok_a", "s1"));
    }
}
