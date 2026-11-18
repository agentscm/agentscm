//! Native sessions that stopped to wait for their person: a permission prompt, a question, or the
//! end of a turn, as that session's own hooks reported it. Any agent can report one: the native
//! hooks of each supported agent where its host exposes the moment, and the generic hook's
//! `waiting` event for the rest. The session's next tool call, prompt or start clears it, so it
//! never outlives the wait it describes. So does the session's end, except for the end of a turn
//! that says what it asks its person: a host that ends its session after every turn still waits
//! for that person's reply ([`Waiting::ended`]). Async native questions are separate: a complete
//! native snapshot identifies which ones still await a matched answer. Activity never answers
//! them, and older snapshots cannot resurrect an answered question.
//!
//! Short-lived like the setup observations beside it: in memory and bounded. A Hub that is
//! stopped leaves its waits in the store for the process that starts next ([`Waiting::save`]);
//! one that dies does not, and then a waiting session reads as not known rather than not
//! waiting. The wait of a session its host named belongs to its person, not to the agent token
//! that reported it (see [`holder`]). The only text kept is the short excerpt of what the wait
//! asks that the hook client sends, with secrets redacted: the end of the agent's last message,
//! its question, or what a permission prompt is for. That last is the host's description of the
//! call, the files an edit names or a shell's command cut before any text it feeds a program;
//! never what an edit writes. It is shown to that session's own person alone. No prompt, other tool input or transcript is kept here, and this
//! table cannot answer a prompt. A wait at a permission prompt or a question may have a pending
//! request in `permission_answer`, which lets that person answer it from the Hub.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::onboarding::same_repository;
use super::store::{ForgeIdentity, Principal, Store, User};
use super::work_lifecycle::IN_FLIGHT_CAP_SECONDS;
use super::{HubError, Result};

/// A wait shorter than this is someone already at the keyboard, so it is never shown.
pub const GRACE_SECONDS: i64 = 60;
/// A wait this old belongs to a session that went away without ending.
const MAX_AGE_SECONDS: i64 = 24 * 60 * 60;
const MAX_ROWS: usize = 4096;
/// Waits one person may have on record, across all their agent tokens. Far above real use: a
/// person's sessions that wait are all listed, however many they run on one token.
const MAX_OWNER_ROWS: usize = 256;
/// The setting a stopping Hub leaves its waits in, for the process that starts next.
const SAVED_KEY: &str = "hub.session_waiting.saved.v1";
/// Ordinary waits in a saved list older than this are not believed: too much went unheard
/// while the Hub was down. Explicit unanswered questions are believed for as long as they are
/// listed, at most a day.
const SAVED_MAX_AGE_SECONDS: i64 = 10 * 60;
/// The longest excerpt of what a wait asks that is kept, in characters.
const MAX_ASKS_CHARS: usize = 500;

/// A native async question, identified by the host's call and question index. The optional
/// excerpt has already been redacted on the person's computer; answers are never sent.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingQuestion {
    pub id: String,
    pub asks: Option<String>,
    pub since: i64,
}

/// All outstanding questions in a completely read native session file. The byte offset
/// orders snapshots so a slow older read cannot bring an answered question back.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuestionSnapshot {
    pub offset: u64,
    pub pending: Vec<PendingQuestion>,
}

impl QuestionSnapshot {
    pub fn valid(&self) -> bool {
        self.offset > 0
            && self.pending.len() <= MAX_OWNER_ROWS
            && self.pending.iter().all(|question| {
                question.id.len() <= 512
                    && serde_json::from_str::<(String, String, usize)>(&question.id).is_ok_and(
                        |(tool, call, _)| {
                            tool == "request_user_input_async"
                                && !call.is_empty()
                                && call.len() <= 256
                                && !call.chars().any(char::is_control)
                        },
                    )
                    && question.since >= 0
                    && question
                        .asks
                        .as_ref()
                        .is_none_or(|asks| asks.chars().count() <= MAX_ASKS_CHARS)
            })
            && self
                .pending
                .iter()
                .map(|q| &q.id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == self.pending.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitReason {
    /// The agent asked to use a tool and its host is showing the permission prompt.
    Permission,
    /// The agent asked the person a question: a choice, or a form an MCP server opened.
    Question,
    /// The agent finished its turn and waits for the person's next message. Its last message
    /// asked for something, or whether it did is not known.
    Idle,
    /// The agent finished its turn and the end of its last message asked its person nothing. It
    /// asks nobody, so it is never shown as a wait: a finished turn is read, not answered.
    Finished,
    /// The agent stopped on its own background work, which will resume it. It asks nobody, so it
    /// is never shown as a wait: it only keeps the Work its session leads from reading as quiet.
    Background,
}

/// The words of `text`, lower-cased, split at anything that is not a letter or a digit.
fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Whether `text` speaks to its reader: "you", "your", "yours" or "yourself" as a word.
pub(super) fn addresses_person(text: &str) -> bool {
    words(text)
        .iter()
        .any(|word| matches!(word.as_str(), "you" | "your" | "yours" | "yourself"))
}

/// Whether the end of an agent's last message asks its person for something. Only its last
/// paragraph is read, and only a real ask counts: a sentence that closes with a question mark,
/// an offer or a request in so many words ("shall I", "do you want", "let me know", "I need
/// your"), a sentence that opens with "which", or one that opens with a request to the person
/// ("Approve…", "Reply with…", "Tell me…"). Words that only report a state ("waiting on CI",
/// "the owner merged it", "blocked by the review", "I need to rerun it") ask nothing, since
/// nearly every report has some and each one put a finished turn in front of its person.
fn asks_person(text: &str) -> bool {
    // Phrases, matched as whole words; a typographic apostrophe spells the same word.
    const ASKS: &[&str] = &[
        "let me know",
        "tell me",
        "send me",
        "should i",
        "shall i",
        "do you want",
        "would you like",
        "if you want",
        "if you'd like",
        "want me to",
        "which one",
        "which of",
        "need your",
        "needs your",
        "need you to",
        "require your",
        "requires your",
        "waiting for your",
        "waiting on your",
        "your call",
        "say go",
    ];
    // Words that open a request to the person, as a sentence's first words.
    const OPENS: &[&str] = &[
        "approve", "confirm", "choose", "pick", "decide", "reply", "respond", "tell", "send",
        "say", "provide", "give", "grant", "paste", "share", "please", "unblock", "sign in",
        "log in", "which",
    ];
    let lower = text.to_lowercase().replace('’', "'");
    // The last paragraph, with the prose before it when it is only a list, a table or code, as
    // in "Which one should I merge?" followed by the choices.
    let mut paragraphs = lower.split('\n').fold(vec![Vec::new()], |mut all, line| {
        match line.trim() {
            "" if all.last().is_some_and(Vec::is_empty) => {}
            "" => all.push(Vec::new()),
            line => all.last_mut().unwrap().push(line),
        }
        all
    });
    paragraphs.retain(|paragraph| !paragraph.is_empty());
    let listed = |paragraph: &Vec<&str>| {
        let fenced = |line: Option<&&str>| line.is_some_and(|line| line.starts_with("```"));
        fenced(paragraph.first())
            || fenced(paragraph.last())
            || paragraph.iter().all(|line| {
                line.starts_with(['-', '*', '+', '|', '`', '>'])
                    || line.split_once(['.', ')']).is_some_and(|(n, _)| {
                        !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())
                    })
            })
    };
    let start = paragraphs
        .iter()
        .rposition(|paragraph| !listed(paragraph))
        .unwrap_or(0);
    let last = paragraphs[start..].concat().join("\n");
    // A question mark closes a sentence where no letter or digit follows it, unlike "?id=1".
    let questions = last.match_indices(['?', '？']).any(|(at, mark)| {
        !last[at + mark.len()..]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric)
    });
    let spoken = |phrase: &str| format!(" {} ", words(phrase).join(" "));
    let said = spoken(&last);
    let opens = last
        .split(['.', '!', '?', ':', ';', '\n'])
        .map(spoken)
        .any(|sentence| OPENS.iter().any(|open| sentence.starts_with(&spoken(open))));
    questions || opens || ASKS.iter().any(|phrase| said.contains(&spoken(phrase)))
}

/// Whether a whitespace-separated token names something instead of saying it: an id, a path or a
/// number, which holds a digit, `_`, `/` or `\`.
fn is_identifier(token: &str) -> bool {
    token.contains(|c: char| c.is_ascii_digit() || matches!(c, '_' | '/' | '\\'))
}

/// Whether `text` is English prose the rule above can read. Its identifiers are set aside; then
/// nearly all of its letters are unaccented, and it uses at least two different words that
/// other languages do not share: a few common ones, or a past tense of six letters or more
/// ("recorded", "merged"). Anything else (another language, a bare word or two, code) is not
/// known to ask nothing.
fn reads_as_english(text: &str) -> bool {
    const COMMON: &[&str] = &[
        "the", "and", "are", "with", "this", "that", "for", "not", "have", "from", "been", "its",
        "were", "has", "done",
    ];
    let prose = text
        .split_whitespace()
        .filter(|token| !is_identifier(token))
        .collect::<Vec<_>>()
        .join(" ");
    let letters = prose.chars().filter(|c| c.is_alphabetic()).count();
    let plain = prose.chars().filter(char::is_ascii_alphabetic).count();
    let mut english = words(&prose);
    english.retain(|word| {
        COMMON.contains(&word.as_str()) || (word.len() >= 6 && word.ends_with("ed"))
    });
    english.sort_unstable();
    english.dedup();
    letters > 0 && plain * 10 >= letters * 9 && english.len() >= 2
}

/// Whether an agent's whole last message asked its person nothing, so its turn's end is read
/// and not answered. It asked nothing when it is only identifiers (an id, a path, a number) or
/// English prose, a report of what was done, whose last paragraph asks nothing: no question, no
/// offer and no request to the person. Anything the rule
/// cannot read (an empty message, another language, a word or two) is not known to ask nothing.
/// The hook client judges the message on the person's machine and sends only this answer; the
/// Hub judges nothing it has not all of.
pub fn turn_asked_nothing(message: &str) -> bool {
    let named = !message.trim().is_empty() && message.split_whitespace().all(is_identifier);
    (named || reads_as_english(message)) && !asks_person(message)
}

impl WaitReason {
    /// Stable for other surfaces to match on.
    pub fn code(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Idle => "idle",
            Self::Finished => "finished",
            Self::Background => "background",
        }
    }

    /// A stop that asks nobody: never shown as a wait, and never counted for a person.
    pub fn asks_nobody(self) -> bool {
        matches!(self, Self::Finished | Self::Background)
    }

    /// The wait a turn's end is. `asked_nothing` is the hook client's answer for the whole last
    /// message. A client that sends none sent only `excerpt`, the message's last characters: it
    /// decides when it is the whole message, and says nothing when it is cut (it then starts
    /// with "…"), because the ask may sit before the cut. Not known means shown.
    pub(super) fn turn_end(asked_nothing: Option<bool>, excerpt: Option<&str>) -> Self {
        let nothing = asked_nothing.unwrap_or_else(|| {
            excerpt.is_some_and(|text| !text.starts_with('…') && turn_asked_nothing(text))
        });
        if nothing { Self::Finished } else { Self::Idle }
    }

    /// How long a wait is believed with no further call from its session. Nothing shows that
    /// background work still runs, so that stop is believed only as long as a running shell call.
    fn keep_seconds(self) -> i64 {
        match self {
            Self::Background => IN_FLIGHT_CAP_SECONDS,
            _ => MAX_AGE_SECONDS,
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        match code {
            "permission" => Some(Self::Permission),
            "question" => Some(Self::Question),
            "idle" => Some(Self::Idle),
            _ => None,
        }
    }

    /// The end of "Your agent <name> …".
    pub fn what(self) -> &'static str {
        match self {
            Self::Permission => "is waiting in its session for your permission to use a tool",
            Self::Question => "is waiting in its session for your answer to a question",
            Self::Idle => "finished its turn and is waiting in its session for your reply",
            Self::Finished => "finished its turn and asked you nothing",
            Self::Background => "stopped on its own background work, which will resume it",
        }
    }
}

/// What the Hub knows of whether the session that leads a Work item waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeadWait {
    /// It reported this wait and has made no call since.
    Waiting(WaitReason),
    NotWaiting,
    /// It may have reported a wait this Hub no longer holds: the Hub restarted, or dropped waits
    /// for room, after that session's last call.
    Unknown,
}

/// One of the viewer's own sessions that has waited for at least [`GRACE_SECONDS`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WaitingSession {
    /// Stable per [`holder`] and native session; neither a secret nor the session ID itself.
    pub key: String,
    pub reason: WaitReason,
    pub since: i64,
    /// The display name of the agent token.
    pub name: String,
    /// The hook's agent (`claude`, `gemini`, ...), for display.
    pub agent: String,
    /// The agent token that reported the wait, to link the Work the session leads. Never shown.
    pub token: String,
    /// The wait is its person's, not that token's: Work that a session of this name leads for the
    /// same person is this session's, whichever of their tokens leads it.
    pub person: bool,
    /// The Hub's name for the session, which Work records as its lead session.
    pub session: String,
    /// A subagent asked; its whole session waits on the answer.
    pub subagent: bool,
    /// What the wait asks, as the hook client excerpted it; third-party text for its person only.
    pub asks: Option<String>,
}

/// Known waits and whether native question coverage is complete for this viewer.
#[derive(Default)]
pub(super) struct WaitingSessions {
    pub waits: Vec<WaitingSession>,
    pub complete: bool,
}

struct Row {
    principal: Principal,
    session: String,
    agent: String,
    reason: WaitReason,
    since: i64,
    /// The subagent that asked, when the host names it.
    asker: Option<String>,
    asks: Option<String>,
    /// Kept under its person ([`holder`]), so it does not end with the token that reported it.
    person: bool,
    /// The host said a dialog is on screen for this wait.
    noticed: bool,
    /// Native question coverage is unknown, from an unreliable source or an unreadable
    /// saved clear marker. Only an authoritative snapshot or clear establishes its state.
    /// It is the one wait exempt from the day limit: a known async question is listed for at
    /// most a day like every other wait, but expiring unknown coverage would be a silent clear.
    uncertain: bool,
}

impl Row {
    fn current(&self, now: i64) -> bool {
        self.since <= now
            && (self.uncertain || now - self.since < self.reason.keep_seconds())
            && (self.person || self.principal.expires_at > now)
    }
}

/// Whose wait a session's is. A session its host named (an ID in UUID form, as the native hooks
/// report) is one session of its person on every call, so its wait is the person's: an agent
/// token is renewed twice a day, and the wait must be found and ended whichever of that
/// person's tokens the next call presents. A name a client chose for itself (`default`, `main`)
/// can be two unrelated sessions of one person, so such a wait stays with the agent token that
/// reported it, and ends with it.
pub(super) fn holder<'a>(principal: &'a Principal, native: &str) -> &'a str {
    if super::store::owner_session(&principal.owner.id, native).is_some() {
        &principal.owner.id
    } else {
        &principal.token_id
    }
}

/// The item key of a wait: stable per [`holder`] and native session, and neither a secret nor
/// the session ID itself.
pub(super) fn wait_key(holder: &str, native: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(holder, native)).unwrap_or_default())
    )
}

fn question_scope(principal: &Principal) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                &principal.owner.identity.provider,
                &principal.owner.identity.host,
                &principal.repository,
                principal.repository_id,
            ))
            .unwrap_or_default()
        )
    )
}

/// The waits of a Hub that stopped, as [`Waiting::save`] leaves them.
#[derive(Serialize, Deserialize)]
struct Saved {
    at: i64,
    /// The mark of [`Waiting::lost_before`].
    lost_before: i64,
    rows: Vec<SavedRow>,
    #[serde(default)]
    questions: Vec<SavedQuestions>,
    #[serde(default)]
    failed_coverage: FailedCoverage,
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedQuestions {
    owner: String,
    holder: String,
    native: String,
    #[serde(default)]
    scope: Option<String>,
    offset: u64,
    at: i64,
}

/// One saved wait: who waits, for what kind of answer and since when. Never what it asks.
#[derive(Serialize, Deserialize)]
struct SavedRow {
    holder: String,
    native: String,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    uncertain: bool,
    session: String,
    agent: String,
    reason: String,
    since: i64,
    asker: Option<String>,
    person: bool,
    #[serde(default)]
    noticed: bool,
    token: String,
    name: String,
    repository: String,
    repository_id: Option<i64>,
    expires_at: i64,
    owner: String,
    provider: String,
    host: String,
    forge_user: i64,
    login: String,
}

/// Keyed by the wait's [`holder`] and the session ID the hook reported.
/// The second field is the time before which a session's last call leaves its wait unknown: the
/// Hub's start, or just after the start of the newest wait it dropped for room.
/// The third is a span of time with the same meaning for a last call inside it: what sessions
/// that were at work before a Hub stopped may have reported while no Hub ran.
#[derive(Default)]
pub struct Waiting(
    Mutex<BTreeMap<RowKey, Row>>,
    AtomicI64,
    (AtomicI64, AtomicI64),
    Mutex<BTreeMap<RowKey, SavedQuestions>>,
    Mutex<FailedCoverage>,
);

type RowKey = (String, String, Option<String>);

/// A clear applies across repositories and token rotation for a named native session. The
/// store retains only an opaque digest; historical clears never consume active wait capacity.
fn clear_key(holder: &str, native: &str) -> String {
    format!(
        "hub.session_waiting.cleared.v1.{:x}",
        Sha256::digest(serde_json::to_vec(&(holder, native)).unwrap())
    )
}

type CoverageKey = (String, String, String);

/// Writes that were not accepted durably. Exact-source retries recover the bounded guard;
/// saturation requires operator reconciliation because some failed keys could not be retained.
#[derive(Clone, Default, Serialize, Deserialize)]
struct FailedCoverage {
    #[serde(with = "coverage_sources")]
    sources: BTreeMap<CoverageKey, u64>,
    saturated: bool,
}

mod coverage_sources {
    use super::*;

    pub(super) fn serialize<S: serde::Serializer>(
        sources: &BTreeMap<CoverageKey, u64>,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        sources.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<BTreeMap<CoverageKey, u64>, D::Error> {
        let mut sources = BTreeMap::<CoverageKey, u64>::new();
        for (key, offset) in Vec::<(CoverageKey, u64)>::deserialize(deserializer)? {
            let kept = sources.entry(key).or_default();
            *kept = (*kept).max(offset);
        }
        Ok(sources)
    }
}

fn coverage_owner(owner: &str) -> String {
    format!("{:x}", Sha256::digest(owner.as_bytes()))
}

fn coverage_key(principal: &Principal, native: &str) -> CoverageKey {
    (
        coverage_owner(&principal.owner.id),
        question_scope(principal),
        wait_key(holder(principal, native), native),
    )
}

impl Waiting {
    fn coverage_write(
        &self,
        key: CoverageKey,
        all_scopes: bool,
        rejected_offset: u64,
        result: Result<()>,
    ) -> Result<()> {
        let mut failed = self.4.lock().map_err(|_| HubError::Busy)?;
        if result.is_err() {
            if failed.sources.len() < MAX_ROWS || failed.sources.contains_key(&key) {
                let kept = failed.sources.entry(key).or_default();
                *kept = (*kept).max(rejected_offset);
            } else {
                failed.saturated = true;
            }
        } else if all_scopes {
            failed
                .sources
                .retain(|kept, _| kept.0 != key.0 || kept.2 != key.2);
        } else {
            failed.sources.remove(&key);
        }
        result
    }

    fn mark_unavailable(
        &self,
        store: &dyn Store,
        principal: &Principal,
        native: &str,
        rejected_offset: u64,
    ) -> Result<()> {
        let key = coverage_key(principal, native);
        // A later generic unavailable report must durably carry a rejected reliable offset
        // before it removes the failed-write guard for this source.
        let rejected_offset = rejected_offset.max(
            self.4
                .lock()
                .map_err(|_| HubError::Busy)?
                .sources
                .get(&key)
                .copied()
                .unwrap_or(0),
        );
        let result = store.native_question_coverage_mark(&key.0, &key.1, &key.2, rejected_offset);
        self.coverage_write(key, false, rejected_offset, result)
    }

    fn resolve_unavailable(
        &self,
        store: &dyn Store,
        principal: &Principal,
        native: &str,
        all_scopes: bool,
        through_offset: Option<u64>,
    ) -> Result<()> {
        let key = coverage_key(principal, native);
        let result = store
            .native_question_coverage_resolve(
                &key.0,
                (!all_scopes).then_some(key.1.as_str()),
                &key.2,
                through_offset,
            )
            .and_then(|resolved| {
                if resolved {
                    Ok(())
                } else {
                    Err(HubError::Busy)
                }
            });
        self.coverage_write(key, all_scopes, 0, result)
    }

    /// The host's notice that a dialog is on screen for `native`. It names no asker and says
    /// nothing, also when the dialog is a question or a subagent's, so a wait at a dialog stays as
    /// it is: its kind, its asker, its start and what it asks. With no such wait on record (the
    /// prompt's own hook did not report it), the notice is the wait.
    pub(super) fn notice(
        &self,
        principal: &Principal,
        native: &str,
        session: &str,
        agent: &str,
        now: i64,
    ) -> Result<()> {
        let kept_by = holder(principal, native);
        if self.dialog_wait(kept_by, native, now).is_none() {
            let reason = WaitReason::Permission;
            self.record(principal, native, session, agent, reason, None, None, now)?;
        }
        let mut rows = self.0.lock().map_err(|_| HubError::Busy)?;
        if let Some(row) = rows.get_mut(&(kept_by.to_owned(), native.to_owned(), None)) {
            row.noticed = true;
        }
        Ok(())
    }

    /// Whether the host said a dialog is on screen for the current wait of `native` at one.
    pub(super) fn noticed(&self, holder: &str, native: &str, now: i64) -> bool {
        self.0.lock().is_ok_and(|rows| {
            rows.get(&(holder.to_owned(), native.to_owned(), None))
                .is_some_and(|row| row.noticed && row.current(now))
        })
    }

    /// Records that `native` (whose Hub session name is `session`) waits for its person, asked by
    /// the subagent `asker` when the host names one, with `asks` saying what for when the hook
    /// sent it. A repeat of the same wait keeps its start and takes the newer `asks`; a repeat by
    /// the same asker that says nothing keeps the older. The host's own notice goes through
    /// [`Self::notice`].
    #[allow(clippy::too_many_arguments)] // The fields of one hook report.
    pub(super) fn record(
        &self,
        principal: &Principal,
        native: &str,
        session: &str,
        agent: &str,
        reason: WaitReason,
        asker: Option<&str>,
        asks: Option<&str>,
        now: i64,
    ) -> Result<()> {
        if principal.read_only || now >= principal.expires_at {
            return Err(HubError::Unauthorized);
        }
        let mut rows = self.0.lock().map_err(|_| HubError::Busy)?;
        rows.retain(|_, row| row.current(now));
        let kept_by = holder(principal, native);
        let person = kept_by == principal.owner.id;
        let key = (kept_by.to_owned(), native.to_owned(), None);
        if !rows.contains_key(&key) {
            let mine = |row: &Row| row.principal.owner.id == principal.owner.id;
            if rows.values().filter(|row| mine(row)).count() >= MAX_OWNER_ROWS {
                // Room is made from waits that ask nobody or the least: a stop that asks nobody
                // first, then the end of a turn, oldest first. A prompt or a question is
                // never dropped for room; with only those left, the new wait is refused.
                let spare = rows
                    .iter()
                    .filter(|(_, row)| {
                        mine(row) && (row.reason.asks_nobody() || row.reason == WaitReason::Idle)
                    })
                    .min_by_key(|(_, row)| (!row.reason.asks_nobody(), row.since))
                    .map(|(key, _)| key.clone());
                // shortcut: one mark for the whole Hub, so one person's overflow also makes
                // other people's older quiet Work read unknown; keep a mark per person if that
                // shows.
                let Some(dropped) = spare.and_then(|spare| rows.remove(&spare)) else {
                    self.lost_before(now + 1);
                    return Err(HubError::Busy);
                };
                self.lost_before(dropped.since + 1);
            }
            if rows.len() >= MAX_ROWS {
                self.lost_before(now + 1);
                return Err(HubError::Busy);
            }
        }
        // The host's idle notice follows a turn's end and says nothing: a turn that asked nothing
        // still asked nothing.
        // shortcut: any turn's end that says nothing is taken for that notice. A second stop
        // with no message and no call since the first would also be kept hidden; pass the
        // notice down from the hook route if a host ever sends one.
        if reason == WaitReason::Idle
            && asks.is_none()
            && rows
                .get(&key)
                .is_some_and(|row| row.reason == WaitReason::Finished)
        {
            return Ok(());
        }
        let (since, said, noticed) = match rows.get(&key) {
            Some(row) if row.reason == reason => (
                row.since,
                row.asks.clone().filter(|_| row.asker.as_deref() == asker),
                row.noticed,
            ),
            _ => (now, None, false),
        };
        rows.insert(
            key,
            Row {
                principal: principal.clone(),
                session: session.to_owned(),
                agent: agent.to_owned(),
                reason,
                since,
                asker: asker.map(str::to_owned),
                asks: asks
                    .map(|text| super::advice::sanitize(text.trim(), MAX_ASKS_CHARS))
                    .filter(|text| !text.is_empty())
                    .or(said),
                person,
                noticed,
                uncertain: false,
            },
        );
        Ok(())
    }

    /// Reconciles an authoritative native question snapshot. Ordinary tool calls, messages,
    /// turn ends and reconnects never clear these rows. Only an accepted complete snapshot
    /// does; an uncertain source can retry its largest accepted or reliably rejected offset,
    /// but never an older one.
    #[allow(clippy::too_many_arguments)] // One native snapshot and its authenticated source.
    pub(super) fn questions(
        &self,
        store: &dyn Store,
        principal: &Principal,
        native: &str,
        session: &str,
        snapshot: &QuestionSnapshot,
        now: i64,
    ) -> Result<()> {
        if principal.read_only || now >= principal.expires_at {
            return Err(HubError::Unauthorized);
        }
        if !snapshot.valid() || snapshot.pending.iter().any(|q| q.since > now) {
            return Err(HubError::Invalid("invalid native question snapshot".into()));
        }
        let kept_by = holder(principal, native);
        let scope = question_scope(principal);
        let key = (kept_by.to_owned(), native.to_owned(), Some(scope.clone()));
        let mut snapshots = self.3.lock().map_err(|_| HubError::Busy)?;
        let mut rows = self.0.lock().map_err(|_| HubError::Busy)?;
        let prior_floor = snapshots.get(&key).cloned();
        let floor_key = key.clone();
        let result = (|| {
            // Serialize the durable read with clear writes, before admitting any pending rows.
            let cleared = store.get_setting(&clear_key(kept_by, native))?.is_some();
            if cleared {
                // A recovered marker verifies the clear for every repository of this source,
                // including rows retained as uncertain after a failed restore lookup.
                self.resolve_unavailable(store, principal, native, true, None)?;
                snapshots.retain(|(owner, source, _), _| owner != kept_by || source != native);
                rows.retain(|(owner, source, _), _| owner != kept_by || source != native);
                return Ok(());
            }
            let coverage = coverage_key(principal, native);
            let rejected =
                store.native_question_coverage_floor(&coverage.0, &coverage.1, &coverage.2)?;
            let failed = self.4.lock().map_err(|_| HubError::Busy)?;
            let failed_offset = failed.sources.get(&coverage).copied();
            let retry_floor = rejected.unwrap_or(0).max(failed_offset.unwrap_or(0));
            if snapshot.offset < retry_floor {
                return Ok(());
            }
            // Saturation lost exact-source ordering evidence. A snapshot cannot reconcile
            // that unknown barrier; an explicit durable clear remains independently valid.
            if failed.saturated {
                return Err(HubError::Busy);
            }
            drop(failed);
            // Empty-source floors can expire only after their coverage is resolved. An
            // unavailable source may have no Row at capacity, and still needs its replay fence.
            let failed = self.4.lock().map_err(|_| HubError::Busy)?;
            let mut expired = BTreeSet::new();
            for (key @ (owner, native, scope), saved) in snapshots.iter() {
                if now - saved.at < MAX_AGE_SECONDS
                    || failed.saturated
                    || rows.iter().any(|((held, source, question), row)| {
                        held == owner
                            && source == native
                            && question.is_some()
                            && row.current(now)
                            && scope
                                .as_ref()
                                .is_some_and(|scope| *scope == question_scope(&row.principal))
                    })
                {
                    continue;
                }
                if let Some(scope) = scope {
                    let coverage = (
                        coverage_owner(&saved.owner),
                        scope.clone(),
                        wait_key(owner, native),
                    );
                    if failed.sources.contains_key(&coverage) {
                        continue;
                    }
                    match store.native_question_coverage_unavailable(
                        &coverage.0,
                        &coverage.1,
                        Some(&coverage.2),
                    ) {
                        Ok(true) => continue,
                        Ok(false) => {}
                        Err(error) => {
                            // No floor is removed on a failed lookup. Record the exact source's
                            // uncertainty so later status reads cannot mistake it for complete.
                            drop(failed);
                            return self.coverage_write(coverage, false, 0, Err(error));
                        }
                    }
                }
                expired.insert(key.clone());
            }
            drop(failed);
            snapshots.retain(|key, _| !expired.contains(key));
            let uncertain = rows.iter().any(|((owner, source, question), row)| {
                owner == kept_by
                    && source == native
                    && question.is_some()
                    && row.uncertain
                    && same_repository(&row.principal, principal)
            }) || rejected.is_some()
                || failed_offset.is_some();
            if let Some(previous) = snapshots.get_mut(&key)
                && (previous.offset > snapshot.offset
                    || (previous.offset == snapshot.offset && !uncertain))
            {
                if previous.offset == snapshot.offset {
                    previous.at = now;
                }
                return Ok(());
            }
            if !snapshots.contains_key(&key)
                && (snapshots.len() >= MAX_ROWS
                    || snapshots
                        .values()
                        .filter(|row| row.owner == principal.owner.id)
                        .count()
                        >= MAX_OWNER_ROWS)
            {
                self.lost_before(now + 1);
                return Err(HubError::Busy);
            }
            // A question a day old is no longer listed, so re-sending it admits no row and counts
            // toward no limit. The offset and floor are recorded as for any accepted snapshot.
            let pending = snapshot
                .pending
                .iter()
                .filter(|question| now - question.since < MAX_AGE_SECONDS)
                .collect::<Vec<_>>();
            let replacing = |(owner, source, question): &RowKey, row: &Row| {
                owner == kept_by
                    && source == native
                    && question.is_some()
                    && same_repository(&row.principal, principal)
            };
            let mut remove = rows
                .iter()
                .filter(|(key, row)| replacing(key, row) || !row.current(now))
                .map(|(key, _)| key.clone())
                .collect::<std::collections::BTreeSet<_>>();
            let remaining = rows
                .iter()
                .filter(|(key, _)| !remove.contains(*key))
                .collect::<Vec<_>>();
            let own = remaining
                .iter()
                .filter(|(_, row)| row.principal.owner.id == principal.owner.id)
                .count();
            let excess = (own + pending.len()).saturating_sub(MAX_OWNER_ROWS);
            let mut spare = remaining
                .iter()
                .filter(|(_, row)| {
                    row.principal.owner.id == principal.owner.id
                        && (row.reason.asks_nobody() || row.reason == WaitReason::Idle)
                })
                .collect::<Vec<_>>();
            spare.sort_by_key(|(_, row)| (!row.reason.asks_nobody(), row.since));
            if spare.len() < excess || remaining.len() + pending.len() - excess > MAX_ROWS {
                self.lost_before(now + 1);
                return Err(HubError::Busy);
            }
            self.resolve_unavailable(store, principal, native, false, Some(snapshot.offset))?;
            for (key, row) in spare.into_iter().take(excess) {
                remove.insert((*key).clone());
                self.lost_before(row.since + 1);
            }
            // Commit replacement only after the whole snapshot fits. Refusal cannot answer an
            // existing question. A refused reliable offset is a retryable lower bound, stored
            // separately from this last successfully accepted snapshot.
            rows.retain(|key, _| !remove.contains(key));
            for question in pending {
                rows.insert(
                    (
                        kept_by.to_owned(),
                        native.to_owned(),
                        Some(serde_json::to_string(&(&scope, &question.id)).unwrap()),
                    ),
                    Row {
                        principal: principal.clone(),
                        session: session.to_owned(),
                        agent: "codex".into(),
                        reason: WaitReason::Question,
                        since: question.since,
                        asker: None,
                        asks: question
                            .asks
                            .as_deref()
                            .map(|text| super::advice::sanitize(text.trim(), MAX_ASKS_CHARS))
                            .filter(|text| !text.is_empty()),
                        person: kept_by == principal.owner.id,
                        noticed: false,
                        uncertain: false,
                    },
                );
            }
            snapshots.insert(
                key,
                SavedQuestions {
                    owner: principal.owner.id.clone(),
                    holder: kept_by.to_owned(),
                    native: native.to_owned(),
                    scope: Some(scope),
                    offset: snapshot.offset,
                    at: now,
                },
            );
            Ok(())
        })();
        if result.is_err() {
            // Pruning may have considered this source resolved before its new report
            // failed admission. Keep its previous ordering fence for the required retry.
            if let Some(prior) = prior_floor {
                snapshots.insert(floor_key, prior);
            }
            // The payload is valid evidence from this authenticated source, even when it
            // cannot be admitted. Persist uncertainty before returning the failed receipt.
            self.mark_unavailable(store, principal, native, snapshot.offset)?;
        }
        result
    }

    /// Records missing native question coverage without answering known questions or
    /// advancing their snapshot offset. A reliable snapshot reconciles this repository later.
    pub(super) fn questions_unavailable(
        &self,
        store: &dyn Store,
        principal: &Principal,
        native: &str,
        session: &str,
        now: i64,
    ) -> Result<()> {
        if principal.read_only || now >= principal.expires_at {
            return Err(HubError::Unauthorized);
        }
        let kept_by = holder(principal, native);
        let scope = question_scope(principal);
        let mut snapshots = self.3.lock().map_err(|_| HubError::Busy)?;
        let marker = store.get_setting(&clear_key(kept_by, native));
        let mut rows = self.0.lock().map_err(|_| HubError::Busy)?;
        if matches!(&marker, Ok(Some(_))) {
            self.resolve_unavailable(store, principal, native, true, None)?;
            snapshots.retain(|(owner, source, _), _| owner != kept_by || source != native);
            rows.retain(|(owner, source, _), _| owner != kept_by || source != native);
            return Ok(());
        }
        self.mark_unavailable(store, principal, native, 0)?;
        rows.retain(|_, row| row.current(now));
        let mut found = false;
        for ((owner, source, question), row) in rows.iter() {
            if owner == kept_by
                && source == native
                && question.is_some()
                && same_repository(&row.principal, principal)
            {
                found = true;
            }
        }
        if !found {
            let mine = |row: &Row| row.principal.owner.id == principal.owner.id;
            if rows.values().filter(|row| mine(row)).count() >= MAX_OWNER_ROWS {
                let spare = rows
                    .iter()
                    .filter(|(_, row)| {
                        mine(row) && (row.reason.asks_nobody() || row.reason == WaitReason::Idle)
                    })
                    .min_by_key(|(_, row)| (!row.reason.asks_nobody(), row.since))
                    .map(|(key, _)| key.clone());
                let Some(dropped) = spare.and_then(|spare| rows.remove(&spare)) else {
                    self.lost_before(now + 1);
                    return Err(HubError::Busy);
                };
                self.lost_before(dropped.since + 1);
            }
            if rows.len() >= MAX_ROWS {
                self.lost_before(now + 1);
                return Err(HubError::Busy);
            }
            rows.insert(
                (
                    kept_by.to_owned(),
                    native.to_owned(),
                    Some(serde_json::to_string(&(&scope, "unavailable")).unwrap()),
                ),
                Row {
                    principal: principal.clone(),
                    session: session.to_owned(),
                    agent: "codex".into(),
                    reason: WaitReason::Question,
                    since: now,
                    asker: None,
                    asks: None,
                    person: kept_by == principal.owner.id,
                    noticed: false,
                    uncertain: true,
                },
            );
        }
        // Retain the last accepted offset as a lower bound; equal-offset reliable recovery
        // is permitted while this source is uncertain, but older snapshots remain stale.
        marker.map(|_| ())
    }

    /// Leaves the current waits in the store for the Hub process that starts next. What a wait
    /// asks is not saved: that text is kept in memory only.
    pub(super) fn save(&self, store: &dyn Store, now: i64) -> Result<()> {
        let snapshots = self.3.lock().map_err(|_| HubError::Busy)?;
        let saved = {
            let rows = self.0.lock().map_err(|_| HubError::Busy)?;
            let rows = rows
                .iter()
                .filter(|(_, row)| row.current(now))
                .map(|((holder, native, question), row)| SavedRow {
                    holder: holder.clone(),
                    native: native.clone(),
                    question: question.clone(),
                    uncertain: row.uncertain,
                    session: row.session.clone(),
                    agent: row.agent.clone(),
                    reason: row.reason.code().to_owned(),
                    since: row.since,
                    asker: row.asker.clone(),
                    person: row.person,
                    noticed: row.noticed,
                    token: row.principal.token_id.clone(),
                    name: row.principal.name.clone(),
                    repository: row.principal.repository.clone(),
                    repository_id: row.principal.repository_id,
                    expires_at: row.principal.expires_at,
                    owner: row.principal.owner.id.clone(),
                    provider: row.principal.owner.identity.provider.clone(),
                    host: row.principal.owner.identity.host.clone(),
                    forge_user: row.principal.owner.identity.user_id,
                    login: row.principal.owner.login.clone(),
                })
                .collect();
            Saved {
                at: now,
                lost_before: self.1.load(Ordering::Relaxed),
                rows,
                questions: snapshots.values().cloned().collect(),
                failed_coverage: self.4.lock().map_err(|_| HubError::Busy)?.clone(),
            }
        };
        drop(snapshots);
        let text = serde_json::to_string(&saved).unwrap_or_default();
        store.set_setting(SAVED_KEY, &text, "hub")
    }

    /// Takes the waits the previous Hub process left when it stopped. The list is taken once,
    /// so a Hub that later dies without saving does not start from an old one. Ordinary
    /// waits are believed only while fresh; explicit questions are believed until a day old.
    /// Without a list, the waits reported to the previous process are
    /// gone: a session that has not called since reads as not known to wait, never as quiet.
    /// With a list, what was reported while no Hub ran is still missing: a session that was at
    /// work shortly before the stop, has no wait in the list and has not called since may have
    /// stopped meanwhile, so it reads as not known until its next call.
    pub(super) fn restore(&self, store: &dyn Store, now: i64) {
        let left = store.get_setting(SAVED_KEY).ok().flatten();
        let taken = left.is_none() || store.set_setting(SAVED_KEY, "", "hub").is_ok();
        let saved = left
            .filter(|_| taken)
            .and_then(|text| serde_json::from_str::<Saved>(&text).ok())
            .filter(|saved| saved.at <= now);
        let Some(saved) = saved else {
            self.lost_before(now);
            return;
        };
        let fresh = now - saved.at < SAVED_MAX_AGE_SECONDS;
        self.lost_before(if fresh { saved.lost_before } else { now });
        let cleared =
            |holder: &str, native: &str| match store.get_setting(&clear_key(holder, native)) {
                Ok(marker) => Some(marker.is_some()),
                Err(_) => {
                    self.lost_before(now);
                    None
                }
            };
        let Ok(mut snapshots) = self.3.lock() else {
            self.lost_before(now);
            return;
        };
        let Ok(mut failed) = self.4.lock() else {
            self.lost_before(now);
            return;
        };
        failed.saturated |=
            saved.failed_coverage.saturated || saved.failed_coverage.sources.len() > MAX_ROWS;
        for (key, offset) in saved.failed_coverage.sources.into_iter().take(MAX_ROWS) {
            if super::store::validate_native_question_coverage_keys(&[&key.0, &key.1, &key.2])
                .is_err()
            {
                failed.saturated = true;
            } else if failed.sources.len() < MAX_ROWS || failed.sources.contains_key(&key) {
                let kept = failed.sources.entry(key).or_default();
                *kept = (*kept).max(offset);
            } else {
                failed.saturated = true;
            }
        }
        drop(failed);
        for snapshot in saved.questions.into_iter().take(MAX_ROWS) {
            // An unreadable clear marker makes pending state uncertain, but the last
            // accepted source offset still excludes delayed older snapshots.
            if snapshot.at <= now && cleared(&snapshot.holder, &snapshot.native) != Some(true) {
                snapshots.insert(
                    (
                        snapshot.holder.clone(),
                        snapshot.native.clone(),
                        snapshot.scope.clone(),
                    ),
                    snapshot,
                );
            }
        }
        let Ok(mut rows) = self.0.lock() else {
            self.lost_before(now);
            return;
        };
        // A session whose last call is older than a running call is believed was not at work.
        let unheard = (saved.at - IN_FLIGHT_CAP_SECONDS, now);
        self.2.0.store(unheard.0, Ordering::Relaxed);
        self.2.1.store(unheard.1, Ordering::Relaxed);
        for row in saved.rows.into_iter().take(MAX_ROWS) {
            let uncertain = match cleared(&row.holder, &row.native) {
                Some(true) => continue,
                Some(false) => row.uncertain && row.question.is_some(),
                None if row.question.is_some() => true,
                None => continue,
            };
            if !fresh && row.question.is_none() {
                continue;
            }
            // Every kind a wait can have, also the ones a hook never reports by name.
            let kinds = [
                WaitReason::Permission,
                WaitReason::Question,
                WaitReason::Idle,
                WaitReason::Finished,
                WaitReason::Background,
            ];
            let Some(reason) = kinds.into_iter().find(|kind| kind.code() == row.reason) else {
                continue;
            };
            let wait = Row {
                principal: Principal {
                    token_id: row.token,
                    name: row.name,
                    repository: row.repository,
                    repository_id: row.repository_id,
                    expires_at: row.expires_at,
                    read_only: false,
                    owner: User {
                        id: row.owner,
                        identity: ForgeIdentity {
                            provider: row.provider,
                            host: row.host,
                            user_id: row.forge_user,
                        },
                        login: row.login,
                        name: None,
                        disabled: false,
                    },
                },
                session: row.session,
                agent: row.agent,
                reason,
                since: row.since,
                asker: row.asker,
                asks: None,
                person: row.person,
                noticed: row.noticed,
                uncertain,
            };
            if wait.current(now) {
                let key = (row.holder, row.native, row.question);
                if !rows.contains_key(&key)
                    && (rows.len() >= MAX_ROWS
                        || rows
                            .values()
                            .filter(|kept| kept.principal.owner.id == wait.principal.owner.id)
                            .count()
                            >= MAX_OWNER_ROWS)
                {
                    self.lost_before(now);
                    continue;
                }
                rows.insert(key, wait);
            }
        }
    }

    /// Says that a wait may be missing for any session whose last call was before `at`: the
    /// waits a Hub dropped for room, or the ones reported to a process that left no list.
    pub(super) fn lost_before(&self, at: i64) {
        self.1.fetch_max(at, Ordering::Relaxed);
    }

    /// Whether anyone's session `native` waits: ordinary activity can skip authentication.
    pub(super) fn has_session(&self, native: &str) -> bool {
        self.0
            .lock()
            .is_ok_and(|rows| rows.keys().any(|(_, session, _)| session == native))
    }

    /// Ends the wait of `native` after a call from it: `all` for a prompt, the session's start, or
    /// an end its person caused by clearing the session (any other end goes through
    /// [`Self::ended`]); otherwise a tool call by `by` (a subagent, or
    /// `None` for the lead), which ends only a wait that subagent asked, or one whose asker the
    /// host did not name. Another subagent working on in parallel does not end it. `holder` is
    /// [`holder`] of the token that made the call: any of a person's tokens ends the wait of a
    /// session its host named, and another person's never does.
    pub(super) fn clear(&self, holder: &str, native: &str, by: Option<&str>, all: bool) {
        let Ok(mut rows) = self.0.lock() else {
            return;
        };
        let key = (holder.to_owned(), native.to_owned(), None);
        if rows
            .get(&key)
            .is_some_and(|row| all || row.asker.is_none() || row.asker.as_deref() == by)
        {
            rows.remove(&key);
        }
    }

    /// An explicit native session clear abandons its questions too. The host continues with
    /// a new native ID; later snapshots of the cleared ID cannot resurrect its old questions.
    pub(super) fn cleared(
        &self,
        store: &dyn Store,
        principal: &Principal,
        native: &str,
        now: i64,
    ) -> Result<()> {
        self.cleared_with_transport(store, principal, native, None, now)
    }

    pub(super) fn cleared_with_transport(
        &self,
        store: &dyn Store,
        principal: &Principal,
        native: &str,
        incarnation: Option<&str>,
        now: i64,
    ) -> Result<()> {
        if principal.read_only || now >= principal.expires_at {
            return Err(HubError::Unauthorized);
        }
        let holder = holder(principal, native);
        let mut snapshots = self.3.lock().map_err(|_| HubError::Busy)?;
        let mut rows = self.0.lock().map_err(|_| HubError::Busy)?;
        // Write before removing known waits. A failed write leaves both maps intact, and a
        // durable marker also suppresses a saved list made just before this clear.
        if incarnation.is_some() {
            store.clear_native_wait_with_transport(
                &principal.token_id,
                native,
                incarnation,
                &clear_key(holder, native),
                now,
            )?;
        } else {
            store.set_setting(&clear_key(holder, native), "1", "hub")?;
        }
        self.resolve_unavailable(store, principal, native, true, None)?;
        snapshots.retain(|(owner, session, _), _| owner != holder || session != native);
        rows.retain(|(owner, session, _), _| owner != holder || session != native);
        Ok(())
    }

    /// Ends the wait of `native` when its session ends, except the end of a turn that says what
    /// it asks its person: a host that ends its session after every turn still waits for that
    /// person's reply. That wait lasts until a prompt, start or tool call in that session, or 24
    /// hours. A dialog closes with its session, and a stop that asks nobody is over, as is a
    /// turn's end with no text (a client that cannot tell what its turn asked, or one restored
    /// after a Hub restart, which keeps no text).
    pub(super) fn ended(&self, holder: &str, native: &str) {
        let Ok(mut rows) = self.0.lock() else {
            return;
        };
        let key = (holder.to_owned(), native.to_owned(), None);
        if rows
            .get(&key)
            .is_some_and(|row| row.reason != WaitReason::Idle || row.asks.is_none())
        {
            rows.remove(&key);
        }
    }

    /// The start, asking subagent and kind of the current wait of `native` at a dialog (a
    /// permission prompt or a question), or `None` when it is gone, replaced by another kind of
    /// wait, or too old. A pending answer is valid only while this still returns what it was
    /// made for.
    pub(super) fn dialog_wait(
        &self,
        holder: &str,
        native: &str,
        now: i64,
    ) -> Option<(i64, Option<String>, WaitReason)> {
        let rows = self.0.lock().ok()?;
        let row = rows.get(&(holder.to_owned(), native.to_owned(), None))?;
        let dialog = matches!(row.reason, WaitReason::Permission | WaitReason::Question);
        (row.current(now) && dialog).then(|| (row.since, row.asker.clone(), row.reason))
    }

    /// The waits of `viewer`'s own sessions in the repository `viewer` reads that have lasted at
    /// least [`GRACE_SECONDS`], oldest first. Missing native question coverage keeps known
    /// waits visible and marks the result incomplete; uncertain restored rows are not asks.
    pub(super) fn owned(&self, store: &dyn Store, viewer: &Principal, now: i64) -> WaitingSessions {
        let Ok(_snapshots) = self.3.lock() else {
            return WaitingSessions::default();
        };
        let owner = coverage_owner(&viewer.owner.id);
        let scope = question_scope(viewer);
        let mut complete = self.4.lock().is_ok_and(|failed| {
            !failed.saturated
                && !failed
                    .sources
                    .keys()
                    .any(|key| key.0 == owner && key.1 == scope)
        });
        complete &= matches!(
            store.native_question_coverage_unavailable(&owner, &scope, None),
            Ok(false)
        );
        let Ok(rows) = self.0.lock() else {
            return WaitingSessions::default();
        };
        if rows.values().any(|row| {
            row.uncertain
                && row.current(now)
                && row.principal.owner.id == viewer.owner.id
                && same_repository(&row.principal, viewer)
        }) {
            complete = false;
        }
        let mut out = rows
            .iter()
            .filter(|(_, row)| {
                row.current(now)
                    && !row.uncertain
                    && !row.reason.asks_nobody()
                    && now - row.since >= GRACE_SECONDS
                    && row.principal.owner.id == viewer.owner.id
                    && same_repository(&row.principal, viewer)
            })
            .map(|((holder, native, question), row)| WaitingSession {
                key: match question {
                    None => wait_key(holder, native),
                    Some(id) => format!(
                        "{:x}",
                        Sha256::digest(
                            serde_json::to_vec(&(holder, native, id)).unwrap_or_default()
                        )
                    ),
                },
                reason: row.reason,
                since: row.since,
                name: row.principal.name.clone(),
                agent: row.agent.clone(),
                token: row.principal.token_id.clone(),
                person: row.person,
                session: row.session.clone(),
                subagent: row.asker.is_some(),
                asks: row.asks.clone(),
            })
            .collect::<Vec<_>>();
        out.sort_by(|a, b| (a.since, &a.key).cmp(&(b.since, &b.key)));
        WaitingSessions {
            waits: out,
            complete,
        }
    }

    /// Whether the session `session` waits, for the Work it leads for `owner` under agent token
    /// `token`, which last called at `last_call`. A wait kept under its person is that session's
    /// whichever of the person's tokens leads the Work: the Work takes a renewed token only at
    /// the session's next Work call, and the wait at its next report. Unlike `owned` it has no grace: the Work waits from the moment
    /// its session stops, and only the notice to its person is held back. With no wait on record
    /// the answer stays unknown until fresh evidence: a question row that is missing or has
    /// unknown coverage is not aged out, so elapsed time cannot establish that the session is not
    /// waiting. A known question is still capped at 24 hours.
    pub(super) fn lead(
        &self,
        owner: &str,
        token: &str,
        session: &str,
        last_call: i64,
        now: i64,
    ) -> LeadWait {
        let Ok(rows) = self.0.lock() else {
            return LeadWait::Unknown;
        };
        let wait = rows
            .iter()
            .filter(|(_, row)| {
                let leads = if row.person {
                    row.principal.owner.id == owner
                } else {
                    row.principal.token_id == token
                };
                leads && row.session == session && row.current(now)
            })
            .min_by_key(|((_, _, question), row)| {
                (!row.uncertain, question.is_none(), row.reason.asks_nobody())
            });
        let unheard = self.2.0.load(Ordering::Relaxed)..self.2.1.load(Ordering::Relaxed);
        let missing = last_call < self.1.load(Ordering::Relaxed) || unheard.contains(&last_call);
        match wait {
            Some((_, row)) if row.uncertain => LeadWait::Unknown,
            Some((_, row)) => LeadWait::Waiting(row.reason),
            None if missing => LeadWait::Unknown,
            None => LeadWait::NotWaiting,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn question_store() -> (tempfile::TempDir, std::sync::Arc<dyn Store>) {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::hub::store::open(&format!(
            "sqlite://{}",
            directory.path().join("hub.sqlite").display()
        ))
        .unwrap();
        (directory, store)
    }

    fn principal(token: &str, owner: &str, repository: &str) -> Principal {
        Principal {
            token_id: token.into(),
            name: format!("{token}-laptop"),
            repository: repository.into(),
            repository_id: Some(7),
            expires_at: 10_000,
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

    fn questions(offset: u64, indexes: impl IntoIterator<Item = usize>) -> QuestionSnapshot {
        QuestionSnapshot {
            offset,
            pending: indexes
                .into_iter()
                .map(|index| PendingQuestion {
                    id: serde_json::to_string(&("request_user_input_async", "call_example", index))
                        .unwrap(),
                    asks: Some(format!("Choose option {index}")),
                    since: 1_000,
                })
                .collect(),
        }
    }

    #[test]
    fn native_questions_follow_each_repository_and_clear_across_restarts() {
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mut one = principal("tok_one", "usr_me", "acme/one");
        one.expires_at = i64::MAX;
        let mut two = one.clone();
        two.token_id = "tok_two".into();
        two.repository = "acme/two".into();
        two.repository_id = Some(8);
        for who in [&one, &two] {
            waiting
                .questions(&*store, who, NATIVE, NATIVE, &questions(100, [0]), 1_000)
                .unwrap();
            assert_eq!(waiting.owned(&*store, who, 1_100).waits.len(), 1);
        }
        assert_ne!(
            waiting.owned(&*store, &one, 1_100).waits[0].key,
            waiting.owned(&*store, &two, 1_100).waits[0].key
        );
        waiting
            .questions(&*store, &one, NATIVE, NATIVE, &questions(200, []), 1_100)
            .unwrap();
        assert!(waiting.owned(&*store, &one, 1_100).waits.is_empty());
        assert_eq!(waiting.owned(&*store, &two, 1_100).waits.len(), 1);
        waiting.save(&*store, 1_100).unwrap();
        let restored = Waiting::default();
        restored.restore(&*store, 1_101);
        one.token_id = "tok_rotated".into();
        restored
            .questions(&*store, &one, NATIVE, NATIVE, &questions(100, [0]), 1_102)
            .unwrap();
        assert!(restored.owned(&*store, &one, 1_120).waits.is_empty());
        assert_eq!(restored.owned(&*store, &two, 1_120).waits.len(), 1);
        // Simulate a saved list racing with clear: the durable clear must win on restart.
        restored.save(&*store, 1_119).unwrap();
        restored.cleared(&*store, &one, NATIVE, 1_120).unwrap();
        let later = 1_121 + 3 * MAX_AGE_SECONDS;
        let restarted = Waiting::default();
        restarted.restore(&*store, later);
        let mut third = one.clone();
        third.repository = "acme/three".into();
        third.repository_id = Some(9);
        for who in [&one, &two, &third] {
            restarted
                .questions(&*store, who, NATIVE, NATIVE, &questions(300, [0]), later)
                .unwrap();
            assert!(restarted.owned(&*store, who, later).waits.is_empty());
        }
    }

    #[test]
    fn failed_clear_marker_read_during_restore_cannot_resurrect_questions() {
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let (directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(100, [0]), 1_000)
            .unwrap();
        waiting
            .record(
                &mine,
                "ordinary",
                "ordinary",
                "codex",
                WaitReason::Permission,
                None,
                None,
                1_000,
            )
            .unwrap();
        waiting.save(&*store, 1_010).unwrap();
        waiting.cleared(&*store, &mine, NATIVE, 1_020).unwrap();
        // Only this marker fails decoding; the saved list can still be read and taken.
        let key = clear_key(holder(&mine, NATIVE), NATIVE);
        let database = rusqlite::Connection::open(directory.path().join("hub.sqlite")).unwrap();
        assert_eq!(
            database
                .execute(
                    "UPDATE hub_settings SET value = ?1 WHERE key = ?2",
                    rusqlite::params![vec![b'1'], &key],
                )
                .unwrap(),
            1
        );
        assert!(store.get_setting(&key).is_err());
        let restored = Waiting::default();
        restored.restore(&*store, 1_100);
        assert_eq!(store.get_setting(SAVED_KEY).unwrap().as_deref(), Some(""));
        assert_eq!(restored.3.lock().unwrap().len(), 1);
        // Ordinary activity after restore cannot turn the unknown question into an all-clear.
        restored
            .record(
                &mine,
                NATIVE,
                NATIVE,
                "codex",
                WaitReason::Finished,
                None,
                None,
                1_101,
            )
            .unwrap();
        for now in [1_102, 1_102 + 3 * MAX_AGE_SECONDS] {
            assert_eq!(
                restored.lead(&mine.owner.id, &mine.token_id, NATIVE, 1_101, now),
                LeadWait::Unknown,
            );
            let shown = restored.owned(&*store, &mine, now);
            assert!(!shown.complete);
            assert!(shown.waits.iter().all(|wait| wait.session == "ordinary"));
        }
        restored.clear(holder(&mine, NATIVE), NATIVE, None, true);
        restored.ended(holder(&mine, NATIVE), NATIVE);
        assert!(!restored.owned(&*store, &mine, 1_102).complete);
        assert!(restored.has_session("ordinary"));
        let mut elsewhere = mine.clone();
        elsewhere.repository = "acme/elsewhere".into();
        elsewhere.repository_id = Some(8);
        assert!(restored.owned(&*store, &elsewhere, 1_102).waits.is_empty());
        let other = principal("tok_other", "usr_other", "acme/widgets");
        assert!(restored.owned(&*store, &other, 1_102).waits.is_empty());
        // Uncertainty survives another save and restore while the marker remains unreadable.
        restored.save(&*store, 1_103).unwrap();
        let restarted = Waiting::default();
        restarted.restore(&*store, 1_104);
        assert!(!restarted.owned(&*store, &mine, 1_105).complete);
        assert_eq!(
            restarted.lead(&mine.owner.id, &mine.token_id, NATIVE, 1_105, 1_106),
            LeadWait::Unknown,
        );
        store.set_setting(&key, "1", "hub").unwrap();
        restarted
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(100, [0]), 1_107)
            .unwrap();
        assert!(!restarted.has_session(NATIVE));
        assert!(restarted.3.lock().unwrap().is_empty());
        assert_eq!(restarted.owned(&*store, &mine, 1_107).waits.len(), 1);
    }

    #[test]
    fn uncertain_restore_requires_an_accepted_snapshot_for_each_repository() {
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let (directory, store) = question_store();
        let waiting = Waiting::default();
        let mut one = principal("tok_one", "usr_me", "acme/one");
        one.expires_at = i64::MAX;
        let mut two = one.clone();
        two.token_id = "tok_two".into();
        two.repository = "acme/two".into();
        two.repository_id = Some(8);
        for who in [&one, &two] {
            waiting
                .questions(&*store, who, NATIVE, NATIVE, &questions(100, [0]), 1_000)
                .unwrap();
        }
        waiting.save(&*store, 1_010).unwrap();
        let key = clear_key(holder(&one, NATIVE), NATIVE);
        let database = rusqlite::Connection::open(directory.path().join("hub.sqlite")).unwrap();
        database.execute(
            "INSERT INTO hub_settings(key, value, updated_at, updated_by) VALUES (?1, ?2, 1010, 'hub')",
            rusqlite::params![&key, vec![b'1']],
        ).unwrap();
        let restored = Waiting::default();
        restored.restore(&*store, 1_100);
        for who in [&one, &two] {
            assert!(!restored.owned(&*store, who, 1_100).complete);
        }
        restored.save(&*store, 1_110).unwrap();
        // A successful absence check cannot answer a question whose state was already unknown.
        database
            .execute("DELETE FROM hub_settings WHERE key = ?1", [&key])
            .unwrap();
        let later = 1_110 + MAX_AGE_SECONDS / 4;
        let restarted = Waiting::default();
        restarted.restore(&*store, later);
        assert_eq!(restarted.3.lock().unwrap().len(), 2);
        for who in [&one, &two] {
            assert!(!restarted.owned(&*store, who, later).complete);
        }
        restarted
            .questions(&*store, &one, NATIVE, NATIVE, &questions(50, []), later)
            .unwrap();
        assert!(!restarted.owned(&*store, &one, later).complete);
        // Unknown rows count toward the existing owner cap and survive a refused replacement.
        restarted
            .questions(
                &*store,
                &one,
                "capacity",
                "capacity",
                &questions(100, 0..MAX_OWNER_ROWS - 2),
                later,
            )
            .unwrap();
        assert_eq!(restarted.0.lock().unwrap().len(), MAX_OWNER_ROWS);
        assert!(matches!(
            restarted.questions(
                &*store,
                &one,
                NATIVE,
                NATIVE,
                &questions(100, [0, 1]),
                later
            ),
            Err(HubError::Busy)
        ));
        assert!(!restarted.owned(&*store, &one, later).complete);
        restarted.cleared(&*store, &one, "capacity", later).unwrap();
        // The same source offset reconciles uncertainty while older offsets stay fenced.
        restarted
            .questions(&*store, &one, NATIVE, NATIVE, &questions(100, []), later)
            .unwrap();
        assert!(restarted.owned(&*store, &one, later).waits.is_empty());
        assert!(!restarted.owned(&*store, &two, later).complete);
        assert_eq!(
            restarted.lead(&one.owner.id, &one.token_id, NATIVE, later, later),
            LeadWait::Unknown
        );
        restarted
            .questions(&*store, &two, NATIVE, NATIVE, &questions(100, [0]), later)
            .unwrap();
        assert_eq!(restarted.owned(&*store, &two, later).waits.len(), 1);
        assert_eq!(
            restarted.lead(&one.owner.id, &one.token_id, NATIVE, later, later),
            LeadWait::Waiting(WaitReason::Question)
        );
    }

    #[test]
    fn uncertain_question_survives_restore_past_a_day_until_a_snapshot() {
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(100, []), 1_000)
            .unwrap();
        waiting
            .questions_unavailable(&*store, &mine, NATIVE, NATIVE, 1_010)
            .unwrap();
        waiting.save(&*store, 1_020).unwrap();
        let later = 1_020 + 3 * MAX_AGE_SECONDS;
        let restored = Waiting::default();
        restored.restore(&*store, later);
        assert!(!restored.owned(&*store, &mine, later).complete);
        assert_eq!(
            restored.lead(&mine.owner.id, &mine.token_id, NATIVE, later, later),
            LeadWait::Unknown
        );
        restored
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(100, []), later)
            .unwrap();
        assert!(restored.owned(&*store, &mine, later).complete);
        assert_ne!(
            restored.lead(&mine.owner.id, &mine.token_id, NATIVE, later, later),
            LeadWait::Unknown
        );
    }

    #[test]
    fn unavailable_question_coverage_survives_restart_without_replacing_known_questions() {
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(100, []), 1_000)
            .unwrap();
        waiting
            .questions_unavailable(&*store, &mine, NATIVE, NATIVE, 1_010)
            .unwrap();
        assert_eq!(waiting.3.lock().unwrap().len(), 1);
        assert_eq!(waiting.0.lock().unwrap().len(), 1);
        assert!(
            waiting
                .0
                .lock()
                .unwrap()
                .values()
                .all(|row| row.uncertain && row.asks.is_none())
        );
        waiting.save(&*store, 1_020).unwrap();
        let later = 1_020 + MAX_AGE_SECONDS / 4;
        let restored = Waiting::default();
        restored.restore(&*store, later);
        assert!(!restored.owned(&*store, &mine, later).complete);
        assert!(restored.owned(&*store, &mine, later).waits.is_empty());
        assert_eq!(
            restored.lead(&mine.owner.id, &mine.token_id, NATIVE, later, later),
            LeadWait::Unknown
        );
        restored
            .questions(
                &*store,
                &mine,
                NATIVE,
                NATIVE,
                &questions(100, 0..MAX_OWNER_ROWS),
                later,
            )
            .unwrap();
        assert_eq!(
            restored.owned(&*store, &mine, later).waits.len(),
            MAX_OWNER_ROWS
        );
        restored
            .questions_unavailable(&*store, &mine, NATIVE, NATIVE, later)
            .unwrap();
        assert_eq!(restored.0.lock().unwrap().len(), MAX_OWNER_ROWS);
        assert!(
            restored
                .0
                .lock()
                .unwrap()
                .values()
                .all(|row| row.asks.is_some() && !row.uncertain)
        );
        let shown = restored.owned(&*store, &mine, later);
        assert!(!shown.complete);
        assert_eq!(shown.waits.len(), MAX_OWNER_ROWS);
        restored
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(50, []), later)
            .unwrap();
        assert_eq!(restored.0.lock().unwrap().len(), MAX_OWNER_ROWS);
        assert!(!restored.owned(&*store, &mine, later).complete);
        restored
            .questions(
                &*store,
                &mine,
                NATIVE,
                NATIVE,
                &questions(100, 0..MAX_OWNER_ROWS),
                later,
            )
            .unwrap();
        assert_eq!(
            restored.owned(&*store, &mine, later).waits.len(),
            MAX_OWNER_ROWS
        );
        assert!(restored.owned(&*store, &mine, later).complete);
    }

    #[test]
    fn unavailable_coverage_read_keeps_known_waits_visible() {
        let (directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        waiting
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(100, [0]),
                1_000,
            )
            .unwrap();
        let before = waiting.owned(&*store, &mine, 1_100);
        assert!(before.complete);
        assert_eq!(before.waits.len(), 1);
        let database = rusqlite::Connection::open(directory.path().join("hub.sqlite")).unwrap();
        database
            .execute_batch(
                "ALTER TABLE hub_native_question_coverage RENAME TO unavailable_coverage;",
            )
            .unwrap();
        let unavailable = waiting.owned(&*store, &mine, 1_100);
        assert!(!unavailable.complete);
        assert_eq!(unavailable.waits, before.waits);
        database
            .execute_batch(
                "ALTER TABLE unavailable_coverage RENAME TO hub_native_question_coverage;",
            )
            .unwrap();
        assert!(waiting.owned(&*store, &mine, 1_100).complete);
    }

    #[test]
    fn unavailable_overflow_stays_scoped_and_durable_until_each_source_recovers() {
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mut one = principal("tok_one", "usr_me", "acme/one");
        one.expires_at = i64::MAX;
        let mut two = one.clone();
        two.token_id = "tok_two".into();
        two.repository = "acme/two".into();
        two.repository_id = Some(8);
        waiting
            .questions(
                &*store,
                &one,
                "full",
                "full",
                &questions(100, 0..MAX_OWNER_ROWS),
                1_000,
            )
            .unwrap();
        waiting
            .questions(&*store, &two, NATIVE, NATIVE, &questions(100, []), 1_000)
            .unwrap();
        for native in [NATIVE, "another-source"] {
            assert!(matches!(
                waiting.questions_unavailable(&*store, &two, native, native, 1_010),
                Err(HubError::Busy)
            ));
        }
        assert_eq!(waiting.0.lock().unwrap().len(), MAX_OWNER_ROWS);
        assert_eq!(
            waiting.owned(&*store, &one, 1_100).waits.len(),
            MAX_OWNER_ROWS
        );
        assert!(!waiting.owned(&*store, &two, 1_100).complete);
        waiting.save(&*store, 1_100).unwrap();
        let later = 1_101 + MAX_AGE_SECONDS / 4;
        let restarted = Waiting::default();
        restarted.restore(&*store, later);
        // Even without a memory slot or a wait row, fresh activity cannot prove complete coverage.
        restarted.clear(holder(&two, NATIVE), NATIVE, None, true);
        assert!(!restarted.owned(&*store, &two, later + 1).complete);
        restarted
            .questions(
                &*store,
                &one,
                "full",
                "full",
                &questions(200, []),
                later + 1,
            )
            .unwrap();
        restarted
            .questions(&*store, &two, NATIVE, NATIVE, &questions(50, []), later + 2)
            .unwrap();
        let native_key = coverage_key(&two, NATIVE);
        assert!(
            store
                .native_question_coverage_unavailable(
                    &native_key.0,
                    &native_key.1,
                    Some(&native_key.2)
                )
                .unwrap()
        );
        assert_eq!(
            restarted
                .3
                .lock()
                .unwrap()
                .get(&(
                    holder(&two, NATIVE).to_owned(),
                    NATIVE.into(),
                    Some(question_scope(&two))
                ))
                .unwrap()
                .offset,
            100
        );
        // An equal-offset empty snapshot resolves only the first source's registry entry.
        restarted
            .questions(
                &*store,
                &two,
                NATIVE,
                NATIVE,
                &questions(100, []),
                later + 2,
            )
            .unwrap();
        assert!(!restarted.owned(&*store, &two, later + 2).complete);
        restarted
            .questions(
                &*store,
                &two,
                "another-source",
                "another-source",
                &questions(100, [0]),
                later + 3,
            )
            .unwrap();
        assert_eq!(restarted.owned(&*store, &two, later + 3).waits.len(), 1);
        let key = coverage_key(&two, NATIVE);
        assert!(
            !store
                .native_question_coverage_unavailable(&key.0, &key.1, None)
                .unwrap()
        );
        assert!(
            store
                .native_question_coverage_mark("not a digest", &key.1, &key.2, 0)
                .is_err()
        );
        assert!(
            store
                .native_question_coverage_unavailable(&key.0, "not a digest", None)
                .is_err()
        );
        assert!(
            store
                .native_question_coverage_resolve(&key.0, None, "not a digest", None)
                .is_err()
        );
    }

    #[test]
    fn refused_reliable_questions_keep_their_repository_unknown_until_the_exact_retry() {
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mut one = principal("tok_one", "usr_me", "acme/one");
        one.expires_at = i64::MAX;
        let mut two = one.clone();
        two.token_id = "tok_two".into();
        two.repository = "acme/two".into();
        two.repository_id = Some(8);
        waiting
            .questions(
                &*store,
                &one,
                "full",
                "full",
                &questions(100, 0..MAX_OWNER_ROWS),
                1_000,
            )
            .unwrap();
        waiting
            .questions(&*store, &two, NATIVE, NATIVE, &questions(100, []), 1_000)
            .unwrap();
        let refused_at = 1_010 + MAX_AGE_SECONDS / 4;
        assert!(matches!(
            waiting.questions(
                &*store,
                &two,
                NATIVE,
                NATIVE,
                &questions(200, [0]),
                refused_at
            ),
            Err(HubError::Busy)
        ));
        assert_eq!(
            waiting.owned(&*store, &one, refused_at + 90).waits.len(),
            MAX_OWNER_ROWS
        );
        waiting.clear(holder(&two, NATIVE), NATIVE, None, true);
        assert!(!waiting.owned(&*store, &two, refused_at + 90).complete);
        let key = coverage_key(&two, NATIVE);
        assert_eq!(
            store
                .native_question_coverage_floor(&key.0, &key.1, &key.2)
                .unwrap(),
            Some(200)
        );
        for offset in [50, 100] {
            waiting
                .questions(
                    &*store,
                    &two,
                    NATIVE,
                    NATIVE,
                    &questions(offset, []),
                    refused_at + 90,
                )
                .unwrap();
            assert!(!waiting.owned(&*store, &two, refused_at + 90).complete);
        }
        waiting.save(&*store, refused_at + 90).unwrap();
        let later = refused_at + 90 + MAX_AGE_SECONDS / 4;
        let restored = Waiting::default();
        restored.restore(&*store, later);
        for offset in [50, 100] {
            restored
                .questions(&*store, &two, NATIVE, NATIVE, &questions(offset, []), later)
                .unwrap();
            assert!(!restored.owned(&*store, &two, later).complete);
        }
        // Native answers in the first repository release capacity without abandoning a session.
        restored
            .questions(
                &*store,
                &one,
                "full",
                "full",
                &questions(200, []),
                later + 1,
            )
            .unwrap();
        assert!(!restored.owned(&*store, &two, later + 1).complete);
        for offset in [50, 100] {
            restored
                .questions(
                    &*store,
                    &two,
                    NATIVE,
                    NATIVE,
                    &questions(offset, []),
                    later + 1,
                )
                .unwrap();
            assert!(!restored.owned(&*store, &two, later + 1).complete);
        }
        restored
            .questions(
                &*store,
                &two,
                NATIVE,
                NATIVE,
                &questions(200, [0]),
                later + 2,
            )
            .unwrap();
        let shown = restored.owned(&*store, &two, later + 2).waits;
        assert!(restored.owned(&*store, &two, later + 2).complete);
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].asks.as_deref(), Some("Choose option 0"));
        assert!(restored.owned(&*store, &one, later + 2).waits.is_empty());
    }

    #[test]
    fn rejected_question_offsets_survive_missing_floor_at_snapshot_caps() {
        for cap in [MAX_OWNER_ROWS, MAX_ROWS] {
            let (_directory, store) = question_store();
            let waiting = Waiting::default();
            let mut mine = principal("tok_a", "usr_me", "acme/widgets");
            mine.expires_at = i64::MAX;
            // Seed accepted empty-source floors, with no active questions. The global case
            // spreads floors across owners so it reaches the global cap independently.
            for index in 0..cap {
                let mut owner = mine.clone();
                if cap == MAX_ROWS && index > 0 {
                    owner.owner.id = format!("usr_{index}");
                    owner.token_id = format!("tok_{index}");
                }
                let native = format!("occupied-{index}");
                let scope = question_scope(&owner);
                waiting.3.lock().unwrap().insert(
                    (
                        holder(&owner, &native).into(),
                        native.clone(),
                        Some(scope.clone()),
                    ),
                    SavedQuestions {
                        owner: owner.owner.id.clone(),
                        holder: holder(&owner, &native).into(),
                        native,
                        scope: Some(scope),
                        offset: 100,
                        at: 1_000,
                    },
                );
            }
            assert!(matches!(
                waiting.questions(
                    &*store,
                    &mine,
                    "new-native",
                    "new-native",
                    &questions(200, [0]),
                    1_010
                ),
                Err(HubError::Busy)
            ));
            assert_eq!(waiting.3.lock().unwrap().len(), cap);
            waiting.save(&*store, 1_020).unwrap();
            let restored = Waiting::default();
            restored.restore(&*store, 1_030);
            // Free a floor, so it cannot be a fresh capacity refusal that protects the source.
            restored
                .cleared(&*store, &mine, "occupied-0", 1_040)
                .unwrap();
            for offset in [50, 100] {
                restored
                    .questions(
                        &*store,
                        &mine,
                        "new-native",
                        "new-native",
                        &questions(offset, []),
                        1_050,
                    )
                    .unwrap();
                let shown = restored.owned(&*store, &mine, 1_060);
                assert!(!shown.complete);
                assert!(shown.waits.is_empty());
                assert_eq!(restored.3.lock().unwrap().len(), cap - 1);
            }
            restored
                .questions(
                    &*store,
                    &mine,
                    "new-native",
                    "new-native",
                    &questions(200, [0]),
                    1_060,
                )
                .unwrap();
            let shown = restored.owned(&*store, &mine, 1_100);
            assert!(shown.complete);
            assert_eq!(shown.waits.len(), 1);
            assert_eq!(restored.3.lock().unwrap().len(), cap);
        }
    }

    #[test]
    fn failed_rejected_offset_write_survives_restart_and_generic_unavailability() {
        let (directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        waiting
            .questions(
                &*store,
                &mine,
                "full",
                "full",
                &questions(100, 0..MAX_OWNER_ROWS),
                1_000,
            )
            .unwrap();
        let database = rusqlite::Connection::open(directory.path().join("hub.sqlite")).unwrap();
        database
            .execute_batch(
                "CREATE TRIGGER reject_coverage BEFORE INSERT ON hub_native_question_coverage
             BEGIN SELECT RAISE(ABORT, 'synthetic coverage write failure'); END;",
            )
            .unwrap();
        assert!(
            waiting
                .questions(
                    &*store,
                    &mine,
                    "new-native",
                    "new-native",
                    &questions(200, [0]),
                    1_010
                )
                .is_err()
        );
        let key = coverage_key(&mine, "new-native");
        assert_eq!(waiting.4.lock().unwrap().sources.get(&key), Some(&200));
        waiting.save(&*store, 1_020).unwrap();
        let restored = Waiting::default();
        let later = 1_030 + MAX_AGE_SECONDS / 4;
        restored.restore(&*store, later);
        database
            .execute_batch("DROP TRIGGER reject_coverage;")
            .unwrap();
        restored
            .questions(&*store, &mine, "full", "full", &questions(200, []), later)
            .unwrap();
        for offset in [50, 100] {
            restored
                .questions(
                    &*store,
                    &mine,
                    "new-native",
                    "new-native",
                    &questions(offset, []),
                    later,
                )
                .unwrap();
            assert!(!restored.owned(&*store, &mine, later).complete);
        }
        restored
            .questions_unavailable(&*store, &mine, "new-native", "new-native", later)
            .unwrap();
        assert_eq!(
            store
                .native_question_coverage_floor(&key.0, &key.1, &key.2)
                .unwrap(),
            Some(200)
        );
        assert!(restored.4.lock().unwrap().sources.is_empty());
        restored
            .questions(
                &*store,
                &mine,
                "new-native",
                "new-native",
                &questions(100, []),
                later,
            )
            .unwrap();
        assert!(!restored.owned(&*store, &mine, later).complete);
        restored
            .questions(
                &*store,
                &mine,
                "new-native",
                "new-native",
                &questions(200, [0]),
                later,
            )
            .unwrap();
        let shown = restored.owned(&*store, &mine, later);
        assert!(shown.complete);
        assert_eq!(shown.waits.len(), 1);
    }

    #[test]
    fn reliable_question_storage_read_failures_remain_unknown_until_exact_recovery() {
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        for marker_read in [true, false] {
            let (directory, store) = question_store();
            let waiting = Waiting::default();
            let mine = principal("tok_a", "usr_me", "acme/widgets");
            waiting
                .questions(&*store, &mine, NATIVE, NATIVE, &questions(100, []), 1_000)
                .unwrap();
            let mut reader = mine.clone();
            reader.read_only = true;
            assert!(matches!(
                waiting.questions(
                    &*store,
                    &reader,
                    NATIVE,
                    NATIVE,
                    &questions(200, [0]),
                    1_010
                ),
                Err(HubError::Unauthorized)
            ));
            assert!(
                waiting
                    .questions(&*store, &mine, NATIVE, NATIVE, &questions(0, [0]), 1_010)
                    .is_err()
            );
            assert!(waiting.owned(&*store, &mine, 1_010).waits.is_empty());
            let database = rusqlite::Connection::open(directory.path().join("hub.sqlite")).unwrap();
            let marker = clear_key(holder(&mine, NATIVE), NATIVE);
            if marker_read {
                database.execute("INSERT INTO hub_settings(key, value, updated_at, updated_by) VALUES (?1, ?2, 1010, 'hub')", rusqlite::params![&marker, vec![b'1']]).unwrap();
            } else {
                database
                    .execute_batch(
                        "ALTER TABLE hub_native_question_coverage RENAME TO unavailable_coverage;",
                    )
                    .unwrap();
            }
            assert!(
                waiting
                    .questions(&*store, &mine, NATIVE, NATIVE, &questions(200, [0]), 1_020)
                    .is_err()
            );
            if marker_read {
                database
                    .execute("DELETE FROM hub_settings WHERE key = ?1", [&marker])
                    .unwrap();
            } else {
                database
                    .execute_batch(
                        "ALTER TABLE unavailable_coverage RENAME TO hub_native_question_coverage;",
                    )
                    .unwrap();
            }
            assert!(!waiting.owned(&*store, &mine, 1_030).complete);
            waiting.save(&*store, 1_040).unwrap();
            let restored = Waiting::default();
            restored.restore(&*store, 1_050);
            restored
                .questions(
                    &*store,
                    &mine,
                    "unrelated",
                    "unrelated",
                    &questions(100, []),
                    1_060,
                )
                .unwrap();
            assert!(!restored.owned(&*store, &mine, 1_060).complete);
            restored
                .questions(&*store, &mine, NATIVE, NATIVE, &questions(200, [0]), 1_070)
                .unwrap();
            assert_eq!(restored.owned(&*store, &mine, 1_070).waits.len(), 1);
        }
    }

    #[test]
    fn failed_coverage_writes_stay_unknown_until_the_exact_source_recovers() {
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let (directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(100, []), 1_000)
            .unwrap();
        let database = rusqlite::Connection::open(directory.path().join("hub.sqlite")).unwrap();
        database
            .execute_batch(
                "CREATE TRIGGER reject_coverage BEFORE INSERT ON hub_native_question_coverage
             BEGIN SELECT RAISE(ABORT, 'synthetic coverage write failure'); END;",
            )
            .unwrap();
        assert!(
            waiting
                .questions_unavailable(&*store, &mine, NATIVE, NATIVE, 1_010)
                .is_err()
        );
        let key = coverage_key(&mine, NATIVE);
        assert!(
            !store
                .native_question_coverage_unavailable(&key.0, &key.1, None)
                .unwrap()
        );
        assert!(!waiting.owned(&*store, &mine, 1_020).complete);
        waiting.save(&*store, 1_030).unwrap();
        let later = 1_040 + 3 * MAX_AGE_SECONDS;
        let restored = Waiting::default();
        restored.restore(&*store, later);
        assert!(!restored.owned(&*store, &mine, later + 10).complete);
        database
            .execute_batch("DROP TRIGGER reject_coverage;")
            .unwrap();
        restored
            .questions(
                &*store,
                &mine,
                "unrelated",
                "unrelated",
                &questions(100, []),
                later + 20,
            )
            .unwrap();
        assert!(!restored.owned(&*store, &mine, later + 20).complete);
        restored
            .questions(
                &*store,
                &mine,
                NATIVE,
                NATIVE,
                &questions(50, []),
                later + 25,
            )
            .unwrap();
        assert!(!restored.owned(&*store, &mine, later + 25).complete);
        assert!(restored.4.lock().unwrap().sources.contains_key(&key));
        restored
            .questions(
                &*store,
                &mine,
                NATIVE,
                NATIVE,
                &questions(100, []),
                later + 30,
            )
            .unwrap();
        assert!(restored.owned(&*store, &mine, later + 30).waits.is_empty());
        assert!(restored.4.lock().unwrap().sources.is_empty());
    }

    #[test]
    fn expired_question_floors_survive_lookup_errors_but_resolved_floors_are_pruned() {
        let (directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        for native in ["native", "resolved"] {
            waiting
                .questions(&*store, &mine, native, native, &questions(100, []), 1_000)
                .unwrap();
        }
        let database = rusqlite::Connection::open(directory.path().join("hub.sqlite")).unwrap();
        database
            .execute_batch(
                "ALTER TABLE hub_native_question_coverage RENAME TO unavailable_coverage;",
            )
            .unwrap();
        let later = 1_000 + 3 * MAX_AGE_SECONDS;
        assert!(
            waiting
                .questions(
                    &*store,
                    &mine,
                    "native",
                    "native",
                    &questions(50, []),
                    later
                )
                .is_err()
        );
        assert_eq!(waiting.3.lock().unwrap().len(), 2);
        database
            .execute_batch(
                "ALTER TABLE unavailable_coverage RENAME TO hub_native_question_coverage;",
            )
            .unwrap();
        assert!(!waiting.owned(&*store, &mine, later).complete);
        waiting
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(50, []),
                later,
            )
            .unwrap();
        assert!(!waiting.owned(&*store, &mine, later).complete);
        let snapshots = waiting.3.lock().unwrap();
        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots.values().next().unwrap().native, "native");
        assert_eq!(snapshots.values().next().unwrap().offset, 100);
        drop(snapshots);
        waiting
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(100, []),
                later,
            )
            .unwrap();
        assert!(waiting.owned(&*store, &mine, later).waits.is_empty());
    }

    #[test]
    fn failed_coverage_guard_is_bounded_and_saturation_survives_restart() {
        let (directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        waiting
            .questions(
                &*store,
                &mine,
                "source-0",
                "source-0",
                &questions(100, []),
                1_000,
            )
            .unwrap();
        let database = rusqlite::Connection::open(directory.path().join("hub.sqlite")).unwrap();
        database
            .execute_batch(
                "CREATE TRIGGER reject_coverage BEFORE INSERT ON hub_native_question_coverage
             BEGIN SELECT RAISE(ABORT, 'synthetic coverage write failure'); END;",
            )
            .unwrap();
        for index in 0..=MAX_ROWS {
            let native = format!("source-{index}");
            assert!(
                waiting
                    .questions_unavailable(&*store, &mine, &native, &native, 1_000)
                    .is_err()
            );
        }
        assert_eq!(waiting.4.lock().unwrap().sources.len(), MAX_ROWS);
        assert!(waiting.4.lock().unwrap().saturated);
        assert!(
            waiting
                .questions(
                    &*store,
                    &mine,
                    "unretained-source",
                    "unretained-source",
                    &questions(200, [0]),
                    1_000,
                )
                .is_err()
        );
        waiting.save(&*store, 1_010).unwrap();
        let restored = Waiting::default();
        let later = 1_020 + 3 * MAX_AGE_SECONDS;
        restored.restore(&*store, later);
        database
            .execute_batch("DROP TRIGGER reject_coverage;")
            .unwrap();
        assert!(matches!(
            restored.questions(
                &*store,
                &mine,
                "source-0",
                "source-0",
                &questions(50, []),
                later,
            ),
            Err(HubError::Busy)
        ));
        assert_eq!(
            restored.3.lock().unwrap().values().next().unwrap().offset,
            100
        );
        assert!(matches!(
            restored.questions(
                &*store,
                &mine,
                "source-0",
                "source-0",
                &questions(100, []),
                later + 10,
            ),
            Err(HubError::Busy)
        ));
        restored
            .cleared(&*store, &mine, "source-0", later + 11)
            .unwrap();
        assert!(restored.3.lock().unwrap().is_empty());
        for offset in [100, 200] {
            assert!(matches!(
                restored.questions(
                    &*store,
                    &mine,
                    "unretained-source",
                    "unretained-source",
                    &questions(offset, []),
                    later + 11,
                ),
                Err(HubError::Busy)
            ));
        }
        let unrelated = principal("tok_other", "usr_other", "acme/other");
        assert!(!restored.owned(&*store, &unrelated, later + 10).complete);
        assert!(restored.4.lock().unwrap().saturated);
    }

    #[test]
    fn async_questions_survive_activity_and_only_newer_matched_snapshots_resolve_them() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let owner = holder(&mine, NATIVE);
        waiting
            .questions(
                &*store,
                &mine,
                NATIVE,
                NATIVE,
                &questions(100, [0, 1]),
                1_000,
            )
            .unwrap();
        assert!(waiting.owned(&*store, &mine, 1_059).waits.is_empty());
        assert_eq!(waiting.owned(&*store, &mine, 1_060).waits.len(), 2);
        // Accepted async tool output, subsequent tools, Hub prompt and clear hooks (which only
        // remove non-question rows), a reconnect and an ordinary session end cannot answer
        // either native question.
        waiting.clear(owner, NATIVE, None, false);
        waiting.clear(owner, NATIVE, Some("helper"), false);
        waiting.clear(owner, NATIVE, None, true);
        waiting.ended(owner, NATIVE);
        waiting
            .record(
                &mine,
                NATIVE,
                NATIVE,
                "codex",
                WaitReason::Finished,
                None,
                None,
                1_010,
            )
            .unwrap();
        assert_eq!(
            waiting.lead(owner, &mine.token_id, NATIVE, 1_010, 1_060),
            LeadWait::Waiting(WaitReason::Question)
        );
        let before = waiting.owned(&*store, &mine, 1_060).waits;
        assert_eq!(before.len(), 2);
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(200, [1]), 1_070)
            .unwrap();
        let remaining = waiting.owned(&*store, &mine, 1_070).waits;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].asks.as_deref(), Some("Choose option 1"));
        assert!(before.iter().any(|q| q.key == remaining[0].key));
        // The older scan completes later; neither its pending answer nor an old empty scan
        // can overwrite the newer native state.
        waiting
            .questions(
                &*store,
                &mine,
                NATIVE,
                NATIVE,
                &questions(100, [0, 1]),
                1_080,
            )
            .unwrap();
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(150, []), 1_080)
            .unwrap();
        assert_eq!(waiting.owned(&*store, &mine, 1_080).waits.len(), 1);
        let other = principal("tok_b", "usr_other", "acme/widgets");
        waiting
            .questions(&*store, &other, NATIVE, NATIVE, &questions(300, []), 1_080)
            .unwrap();
        waiting
            .questions(
                &*store,
                &mine,
                "different-session",
                "different-session",
                &questions(300, []),
                1_080,
            )
            .unwrap();
        assert_eq!(waiting.owned(&*store, &mine, 1_080).waits.len(), 1);
        let renewed = principal("tok_renewed", "usr_me", "acme/widgets");
        waiting
            .questions(
                &*store,
                &renewed,
                NATIVE,
                NATIVE,
                &questions(300, []),
                1_090,
            )
            .unwrap();
        assert!(waiting.owned(&*store, &mine, 1_090).waits.is_empty());
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(200, [1]), 1_100)
            .unwrap();
        assert!(waiting.owned(&*store, &mine, 1_100).waits.is_empty());
    }

    #[test]
    fn historical_clears_leave_active_snapshot_capacity_and_owner_limits_intact() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        for index in 0..MAX_OWNER_ROWS + 1 {
            waiting
                .cleared(&*store, &mine, &format!("native-{index}"), 1_000)
                .unwrap();
        }
        assert!(waiting.3.lock().unwrap().is_empty());
        for index in 0..MAX_OWNER_ROWS {
            let native = format!("active-{index}");
            waiting
                .questions(
                    &*store,
                    &mine,
                    &native,
                    &native,
                    &questions(100, [0]),
                    1_000,
                )
                .unwrap();
        }
        assert_eq!(waiting.3.lock().unwrap().len(), MAX_OWNER_ROWS);
        assert!(matches!(
            waiting.questions(
                &*store,
                &mine,
                "excess",
                "excess",
                &questions(100, [0]),
                1_000
            ),
            Err(HubError::Busy)
        ));
        let other = principal("tok_b", "usr_other", "acme/widgets");
        waiting
            .questions(
                &*store,
                &other,
                "active",
                "active",
                &questions(100, [0]),
                1_000,
            )
            .unwrap();
        waiting.cleared(&*store, &mine, "active-0", 1_100).unwrap();
        waiting
            .questions(
                &*store,
                &mine,
                "excess",
                "excess",
                &questions(100, [0]),
                1_100,
            )
            .unwrap();
        assert_eq!(
            waiting.owned(&*store, &mine, 1_100).waits.len(),
            MAX_OWNER_ROWS
        );
        assert_eq!(waiting.owned(&*store, &other, 1_100).waits.len(), 1);
    }

    #[test]
    fn failed_clear_storage_keeps_known_questions_and_watermarks() {
        let (directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        waiting
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(100, [0]),
                1_000,
            )
            .unwrap();
        let database = rusqlite::Connection::open(directory.path().join("hub.sqlite")).unwrap();
        database
            .execute_batch(
                "CREATE TRIGGER reject_clear BEFORE INSERT ON hub_settings
             WHEN NEW.key LIKE 'hub.session_waiting.cleared.%'
             BEGIN SELECT RAISE(ABORT, 'synthetic clear write failure'); END;",
            )
            .unwrap();
        assert!(waiting.cleared(&*store, &mine, "native", 1_100).is_err());
        assert_eq!(waiting.owned(&*store, &mine, 1_100).waits.len(), 1);
        assert_eq!(waiting.3.lock().unwrap().len(), 1);
        database
            .execute_batch("DROP TRIGGER reject_clear;")
            .unwrap();
        waiting.cleared(&*store, &mine, "native", 1_100).unwrap();
        assert!(waiting.owned(&*store, &mine, 1_100).waits.is_empty());
        assert!(waiting.3.lock().unwrap().is_empty());
    }

    #[test]
    fn explicit_clear_prevents_later_history_from_reopening_abandoned_questions() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        waiting
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(100, [0]),
                1_000,
            )
            .unwrap();
        waiting.cleared(&*store, &mine, "native", 1_010).unwrap();
        waiting
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(200, [0]),
                1_020,
            )
            .unwrap();
        assert!(waiting.owned(&*store, &mine, 1_100).waits.is_empty());
        waiting
            .cleared(&*store, &mine, "not-yet-polled", 1_010)
            .unwrap();
        waiting
            .questions(
                &*store,
                &mine,
                "not-yet-polled",
                "not-yet-polled",
                &questions(200, [0]),
                1_020,
            )
            .unwrap();
        assert!(waiting.owned(&*store, &mine, 1_100).waits.is_empty());
        waiting
            .questions(
                &*store,
                &mine,
                "new-native",
                "new-native",
                &questions(200, [0]),
                1_020,
            )
            .unwrap();
        assert_eq!(waiting.owned(&*store, &mine, 1_100).waits.len(), 1);
    }

    #[test]
    fn explicit_native_questions_are_listed_for_a_day_or_until_their_matched_reply() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let before = questions(100, [0, 1]);
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &before, 1_000)
            .unwrap();
        // Like every other wait, a known question is listed for a day and not a second more.
        assert_eq!(
            waiting
                .owned(&*store, &mine, 1_000 + MAX_AGE_SECONDS - 1)
                .waits
                .len(),
            2
        );
        assert!(
            waiting
                .owned(&*store, &mine, 1_000 + MAX_AGE_SECONDS)
                .waits
                .is_empty()
        );
        let later = 1_000 + MAX_AGE_SECONDS / 2;
        assert_eq!(waiting.owned(&*store, &mine, later).waits.len(), 2);
        waiting.save(&*store, later).unwrap();
        let restored = Waiting::default();
        restored.restore(&*store, later + 1);
        restored
            .questions(
                &*store,
                &mine,
                NATIVE,
                NATIVE,
                &questions(50, []),
                later + 2,
            )
            .unwrap();
        assert_eq!(restored.owned(&*store, &mine, later + 2).waits.len(), 2);
        waiting.clear(holder(&mine, NATIVE), NATIVE, None, true);
        waiting.ended(holder(&mine, NATIVE), NATIVE);
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(50, []), later)
            .unwrap();
        assert_eq!(waiting.owned(&*store, &mine, later).waits.len(), 2);
        // A new Hub can first learn a question that is under a day old.
        let restarted = Waiting::default();
        restarted
            .questions(&*store, &mine, NATIVE, NATIVE, &before, later)
            .unwrap();
        assert_eq!(restarted.owned(&*store, &mine, later).waits.len(), 2);
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(200, [1]), later)
            .unwrap();
        assert_eq!(waiting.owned(&*store, &mine, later).waits.len(), 1);
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(300, []), later)
            .unwrap();
        assert!(waiting.owned(&*store, &mine, later).waits.is_empty());
    }

    #[test]
    fn explicit_questions_survive_hub_downtime_within_a_day() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(100, [0]), 1_000)
            .unwrap();
        waiting
            .record(
                &mine,
                "ordinary",
                "ordinary",
                "codex",
                WaitReason::Permission,
                None,
                None,
                1_000,
            )
            .unwrap();
        waiting.save(&*store, 1_010).unwrap();
        // Far longer than ordinary waits are believed, still under a day.
        let later = 1_010 + MAX_AGE_SECONDS / 2;
        assert!(later - 1_010 > SAVED_MAX_AGE_SECONDS);
        let restored = Waiting::default();
        restored.restore(&*store, later);
        let remaining = restored.owned(&*store, &mine, later).waits;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].reason, WaitReason::Question);
        restored
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(50, []), later)
            .unwrap();
        assert_eq!(restored.owned(&*store, &mine, later).waits.len(), 1);
        restored
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(200, []), later)
            .unwrap();
        assert!(restored.owned(&*store, &mine, later).waits.is_empty());
        // After a day the question is gone, as every other wait is.
        let expired = Waiting::default();
        expired.restore(&*store, 1_000 + MAX_AGE_SECONDS);
        assert!(
            expired
                .owned(&*store, &mine, 1_000 + MAX_AGE_SECONDS)
                .waits
                .is_empty()
        );
    }

    #[test]
    fn a_snapshot_does_not_readmit_a_question_a_day_old() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        const NATIVE: &str = "55555555-5555-4555-8555-555555555555";
        let now = 1_000 + MAX_AGE_SECONDS;
        let mut other = questions(100, [0]);
        other.pending[0].since = now - GRACE_SECONDS;
        waiting
            .questions(&*store, &mine, "other", "other", &other, now)
            .unwrap();
        // Old questions that would exceed the limit if admitted, and one that is current.
        let mut snapshot = questions(100, 0..MAX_OWNER_ROWS);
        snapshot.pending.last_mut().unwrap().since = now - GRACE_SECONDS;
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &snapshot, now)
            .unwrap();
        assert_eq!(waiting.owned(&*store, &mine, now).waits.len(), 2);
        assert_eq!(waiting.0.lock().unwrap().len(), 2);
        // The offset is recorded as for any accepted snapshot, so an older one is still stale.
        waiting
            .questions(&*store, &mine, NATIVE, NATIVE, &questions(50, [0]), now)
            .unwrap();
        assert_eq!(waiting.owned(&*store, &mine, now).waits.len(), 2);
    }

    #[test]
    fn question_snapshot_replacement_is_atomic_at_capacity_and_survives_restart() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        waiting
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(100, 0..MAX_OWNER_ROWS),
                1_000,
            )
            .unwrap();
        waiting
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(200, MAX_OWNER_ROWS..2 * MAX_OWNER_ROWS),
                1_010,
            )
            .unwrap();
        assert_eq!(
            waiting.owned(&*store, &mine, 1_060).waits.len(),
            MAX_OWNER_ROWS
        );
        let before = waiting.owned(&*store, &mine, 1_060).waits;
        assert!(
            waiting
                .questions(
                    &*store,
                    &mine,
                    "second",
                    "second",
                    &questions(100, [0]),
                    1_020
                )
                .is_err()
        );
        assert!(!waiting.owned(&*store, &mine, 1_060).complete);
        assert_eq!(waiting.0.lock().unwrap().len(), before.len());
        waiting.save(&*store, 1_070).unwrap();
        let saved = store.get_setting(SAVED_KEY).unwrap().unwrap();
        assert!(!saved.contains("Choose option"));
        let restored = Waiting::default();
        restored.restore(&*store, 1_080);
        assert!(!restored.owned(&*store, &mine, 1_080).complete);
        assert_eq!(restored.0.lock().unwrap().len(), MAX_OWNER_ROWS);
        restored.clear("tok_a", "native", None, true);
        restored.ended("tok_a", "native");
        assert!(!restored.owned(&*store, &mine, 1_080).complete);
        assert_eq!(restored.0.lock().unwrap().len(), MAX_OWNER_ROWS);
        restored
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(150, []),
                1_090,
            )
            .unwrap();
        assert!(!restored.owned(&*store, &mine, 1_090).complete);
        assert_eq!(restored.0.lock().unwrap().len(), MAX_OWNER_ROWS);
        restored
            .questions(
                &*store,
                &mine,
                "native",
                "native",
                &questions(300, []),
                1_100,
            )
            .unwrap();
        assert!(!restored.owned(&*store, &mine, 1_100).complete);
        assert!(restored.0.lock().unwrap().is_empty());
        restored
            .questions(
                &*store,
                &mine,
                "second",
                "second",
                &questions(100, [0]),
                1_101,
            )
            .unwrap();
        assert_eq!(restored.owned(&*store, &mine, 1_101).waits.len(), 1);
    }

    #[test]
    fn a_wait_shows_after_the_grace_only_to_its_owner_in_its_repository_until_cleared() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let theirs = principal("tok_b", "usr_them", "acme/widgets");
        let elsewhere = principal("tok_c", "usr_me", "acme/other");
        let wait = |who: &Principal, native: &str, reason, now| {
            waiting
                .record(who, native, native, "claude", reason, None, None, now)
                .unwrap();
        };
        wait(&mine, "s1", WaitReason::Permission, 1_000);
        wait(&theirs, "s2", WaitReason::Permission, 1_000);
        wait(&elsewhere, "s3", WaitReason::Question, 1_000);
        // Someone at the keyboard answers within the grace: nothing to show yet.
        assert!(
            waiting
                .owned(&*store, &mine, 1_000 + GRACE_SECONDS - 1)
                .waits
                .is_empty()
        );
        let shown = waiting.owned(&*store, &mine, 1_000 + GRACE_SECONDS).waits;
        assert_eq!(shown.len(), 1);
        assert_eq!(
            (shown[0].reason, shown[0].since, shown[0].session.as_str()),
            (WaitReason::Permission, 1_000, "s1")
        );
        assert!(!shown[0].key.contains("s1") && !shown[0].key.contains("tok_a"));
        // A repeat of the same wait keeps its start; a different wait starts again.
        wait(&mine, "s1", WaitReason::Permission, 1_100);
        assert_eq!(waiting.owned(&*store, &mine, 1_200).waits[0].since, 1_000);
        wait(&mine, "s1", WaitReason::Idle, 1_150);
        assert!(waiting.owned(&*store, &mine, 1_200).waits.is_empty());
        assert_eq!(
            waiting.owned(&*store, &mine, 1_210).waits[0].reason,
            WaitReason::Idle
        );
        // Only the session's own token clears it.
        assert!(waiting.has_session("s1"));
        waiting.clear("tok_b", "s1", None, true);
        assert_eq!(waiting.owned(&*store, &mine, 1_210).waits.len(), 1);
        waiting.clear("tok_a", "s1", Some("any-subagent"), false);
        assert!(!waiting.has_session("s1"));
        assert!(waiting.owned(&*store, &mine, 1_210).waits.is_empty());
    }

    #[test]
    fn a_repeat_that_says_nothing_keeps_what_the_wait_asked() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let record = |asker, asks, now| {
            waiting
                .record(
                    &mine,
                    "s",
                    "s",
                    "claude",
                    WaitReason::Permission,
                    asker,
                    asks,
                    now,
                )
                .unwrap();
            waiting.owned(&*store, &mine, 2_000).waits[0].asks.clone()
        };
        let asked = record(None, Some("Publish the crate"), 1_000);
        assert_eq!(asked.as_deref(), Some("Publish the crate"));
        // The host's notice after its own prompt carries no text.
        assert_eq!(record(None, None, 1_006), asked);
        assert_eq!(
            record(None, Some("Tag it"), 1_010).as_deref(),
            Some("Tag it")
        );
        // Another asker's prompt is another question.
        assert_eq!(record(Some("sub"), None, 1_020), None);
    }

    #[test]
    fn the_hosts_notice_leaves_a_question_a_question() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let record = |reason, asks, now| {
            waiting
                .record(&mine, "s", "s", "claude", reason, None, asks, now)
                .unwrap();
            waiting.dialog_wait("tok_a", "s", now)
        };
        let question = Some((1_000, None, WaitReason::Question));
        assert_eq!(
            record(WaitReason::Question, Some("Which color?"), 1_000),
            question
        );
        let notice = |now| {
            waiting.notice(&mine, "s", "s", "claude", now).unwrap();
            waiting.dialog_wait("tok_a", "s", now)
        };
        // The notice says a dialog is on screen, not which: same wait, same start.
        assert_eq!(notice(1_006), question);
        assert_eq!(
            waiting.owned(&*store, &mine, 2_000).waits[0]
                .asks
                .as_deref(),
            Some("Which color?")
        );
        // A permission prompt is a new wait, also one that says nothing, and the end of a turn
        // is no dialog at all.
        assert_eq!(
            record(WaitReason::Permission, None, 1_010),
            Some((1_010, None, WaitReason::Permission))
        );
        assert_eq!(record(WaitReason::Idle, None, 1_020), None);
        // With no dialog's wait on record, the notice is the wait.
        assert_eq!(notice(1_025), Some((1_025, None, WaitReason::Permission)));
        // A subagent's prompt keeps its asker through the notice, which names none. The lead's
        // own prompt, which says nothing either, replaces it.
        let report = |asker, now| {
            let reason = WaitReason::Permission;
            waiting
                .record(&mine, "s", "s", "claude", reason, asker, None, now)
                .unwrap();
            waiting.dialog_wait("tok_a", "s", now)
        };
        let asked = Some((1_025, Some("sub".to_owned()), WaitReason::Permission));
        assert_eq!(report(Some("sub"), 1_030), asked);
        assert_eq!(notice(1_036), asked);
        assert_eq!(
            report(None, 1_040),
            Some((1_025, None, WaitReason::Permission))
        );
    }

    #[test]
    fn a_subagents_wait_outlasts_its_siblings_work_but_not_its_own_or_a_prompt() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let ask = || {
            waiting
                .record(
                    &mine,
                    "s",
                    "s",
                    "claude",
                    WaitReason::Permission,
                    Some("sub-a"),
                    None,
                    1_000,
                )
                .unwrap();
        };
        ask();
        // A sibling subagent and the lead keep working while sub-a waits.
        waiting.clear("tok_a", "s", Some("sub-b"), false);
        waiting.clear("tok_a", "s", None, false);
        let shown = waiting.owned(&*store, &mine, 1_100).waits;
        assert!(shown.len() == 1 && shown[0].subagent);
        waiting.clear("tok_a", "s", Some("sub-a"), false);
        assert!(waiting.owned(&*store, &mine, 1_100).waits.is_empty());
        ask();
        waiting.clear("tok_a", "s", None, true);
        assert!(waiting.owned(&*store, &mine, 1_100).waits.is_empty());
    }

    #[test]
    fn old_expired_read_only_and_excess_waits_are_dropped() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        waiting
            .record(
                &mine,
                "old",
                "old",
                "codex",
                WaitReason::Idle,
                None,
                None,
                0,
            )
            .unwrap();
        assert_eq!(
            waiting
                .owned(&*store, &mine, MAX_AGE_SECONDS - 1)
                .waits
                .len(),
            1
        );
        assert!(
            waiting
                .owned(&*store, &mine, MAX_AGE_SECONDS)
                .waits
                .is_empty()
        );
        waiting.clear("tok_a", "old", None, true);
        let mut expiring = mine.clone();
        expiring.expires_at = 2_000;
        waiting
            .record(
                &expiring,
                "exp",
                "exp",
                "codex",
                WaitReason::Idle,
                None,
                None,
                1_000,
            )
            .unwrap();
        assert!(waiting.owned(&*store, &mine, 2_000).waits.is_empty());
        let mut reader = mine.clone();
        reader.read_only = true;
        assert!(matches!(
            waiting.record(
                &reader,
                "r",
                "r",
                "codex",
                WaitReason::Idle,
                None,
                None,
                1_000
            ),
            Err(HubError::Unauthorized)
        ));
        // One person's share is counted across their tokens. Past it, room is made from the end
        // of a turn, oldest first, and never from a question or a prompt.
        waiting.clear("tok_a", "exp", None, true);
        let mut other = principal("tok_b", "usr_me", "acme/widgets");
        other.expires_at = i64::MAX;
        let record = |who: &Principal, native: &str, reason, at| {
            waiting.record(who, native, native, "cursor", reason, Some("x"), None, at)
        };
        record(&mine, "idle0", WaitReason::Idle, 1_000).unwrap();
        record(&other, "idle1", WaitReason::Idle, 1_001).unwrap();
        for n in 2..MAX_OWNER_ROWS {
            let at = 1_000 + i64::try_from(n).unwrap();
            record(&mine, &format!("s{n}"), WaitReason::Question, at).unwrap();
        }
        record(&other, "new0", WaitReason::Permission, 2_000).unwrap();
        let shown = waiting.owned(&*store, &mine, 5_000).waits;
        assert_eq!(shown.len(), MAX_OWNER_ROWS);
        assert!(shown.iter().all(|w| w.session != "idle0" && w.subagent));
        // The dropped wait is lost, not ended: its session reads as unknown, the kept ones wait.
        assert_eq!(
            waiting.lead("usr_me", "tok_a", "idle0", 1_000, 5_000),
            LeadWait::Unknown
        );
        assert_eq!(
            waiting.lead("usr_me", "tok_a", "s2", 1_002, 5_000),
            LeadWait::Waiting(WaitReason::Question)
        );
        record(&mine, "new1", WaitReason::Permission, 2_001).unwrap();
        assert_eq!(
            waiting.lead("usr_me", "tok_b", "idle1", 1_001, 5_000),
            LeadWait::Unknown
        );
        // With only questions and prompts left, a new wait is refused and none is dropped.
        assert!(matches!(
            record(&mine, "new2", WaitReason::Question, 2_002),
            Err(HubError::Busy)
        ));
        assert_eq!(
            waiting.lead("usr_me", "tok_a", "s2", 1_002, 5_000),
            LeadWait::Waiting(WaitReason::Question)
        );
    }

    #[test]
    fn the_wait_of_a_session_its_host_named_follows_its_person_to_a_renewed_token() {
        let (_directory, store) = question_store();
        const NATIVE: &str = "0a1b2c3d-0000-4000-8000-000000000001";
        let waiting = Waiting::default();
        let mut old = principal("tok_old", "usr_me", "acme/widgets");
        old.expires_at = 2_000;
        let new = principal("tok_new", "usr_me", "acme/widgets");
        let theirs = principal("tok_x", "usr_them", "acme/widgets");
        let record = |native: &str| {
            let reason = WaitReason::Permission;
            waiting
                .record(&old, native, native, "claude", reason, None, None, 1_000)
                .unwrap();
        };
        record(NATIVE);
        // The token that reported the wait has ended. The session still waits, under one key,
        // and the Work it leads reads as waiting under the old token and the renewed one.
        let shown = waiting.owned(&*store, &new, 3_000).waits;
        assert_eq!(shown.len(), 1);
        assert_eq!(shown[0].key, wait_key("usr_me", NATIVE));
        assert!(shown[0].person);
        for token in ["tok_old", "tok_new"] {
            assert_eq!(
                waiting.lead("usr_me", token, NATIVE, 990, 3_000),
                LeadWait::Waiting(WaitReason::Permission)
            );
        }
        assert_eq!(
            waiting.lead("usr_them", "tok_x", NATIVE, 990, 3_000),
            LeadWait::NotWaiting
        );
        // Another person's call does not end it. The session's next call does, under whichever
        // token its person's computer holds by then.
        waiting.clear(holder(&theirs, NATIVE), NATIVE, None, true);
        assert!(waiting.has_session(NATIVE));
        waiting.clear(holder(&new, NATIVE), NATIVE, None, false);
        assert!(!waiting.has_session(NATIVE));
        // A name a client chose for itself can be two sessions of one person: its wait stays
        // with the token that reported it, and ends with it.
        record("default");
        assert_eq!(holder(&old, "default"), "tok_old");
        waiting.clear(holder(&new, "default"), "default", None, true);
        assert!(waiting.has_session("default"));
        assert_eq!(waiting.owned(&*store, &new, 1_900).waits.len(), 1);
        assert!(!waiting.owned(&*store, &new, 1_900).waits[0].person);
        assert!(waiting.owned(&*store, &new, 2_000).waits.is_empty());
    }

    #[test]
    fn a_stopped_hub_leaves_its_waits_for_the_one_that_starts_next() {
        const NATIVE: &str = "0a1b2c3d-0000-4000-8000-000000000002";
        // The Hub stops at `STOP` and the next process starts five seconds later.
        const STOP: i64 = 100_000;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.sqlite");
        let store = crate::hub::store::open(&format!("sqlite://{}", path.display())).unwrap();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        let stopping = Waiting::default();
        let record = |native: &str, reason, asks| {
            stopping
                .record(
                    &mine,
                    native,
                    native,
                    "claude",
                    reason,
                    None,
                    asks,
                    STOP - 100,
                )
                .unwrap();
        };
        record(NATIVE, WaitReason::Permission, Some("Push it?"));
        stopping
            .notice(&mine, NATIVE, NATIVE, "claude", STOP - 94)
            .unwrap();
        record("idle", WaitReason::Idle, Some("Shall I merge it?"));
        record("busy", WaitReason::Background, None);
        record("done", WaitReason::Finished, None);
        // A turn's end whose session then ended is kept, and is saved like any other wait.
        stopping.ended("tok_a", "idle");
        stopping.lost_before(STOP - 50_000);
        stopping.save(&*store, STOP).unwrap();
        // The next process knows the same waits, from the same start, under the same keys.
        let started = Waiting::default();
        started.restore(&*store, STOP + 5);
        let (before, after) = (
            stopping.owned(&*store, &mine, STOP + 100).waits,
            started.owned(&*store, &mine, STOP + 100).waits,
        );
        assert_eq!(after.len(), 2);
        for (was, is) in before.iter().zip(&after) {
            let same = |wait: &WaitingSession| {
                (
                    wait.key.clone(),
                    wait.reason,
                    wait.since,
                    wait.person,
                    wait.token.clone(),
                )
            };
            assert_eq!(same(was), same(is));
        }
        // What a wait asks was never stored. That the host showed its dialog was.
        assert!(after.iter().all(|wait| wait.asks.is_none()));
        assert_eq!(
            started.dialog_wait("usr_me", NATIVE, STOP + 100),
            Some((STOP - 100, None, WaitReason::Permission))
        );
        assert!(started.noticed("usr_me", NATIVE, STOP + 100));
        assert!(!started.noticed("tok_a", "idle", STOP + 100));
        // A session that stopped on its own work still does. One with no wait on record is as
        // known as it was (quiet since the old mark, not known before it), unless it was at
        // work in the hour before the stop and has not called since: it may have stopped while
        // no Hub ran.
        let lead =
            |session, last_call| started.lead("usr_me", "tok_a", session, last_call, STOP + 100);
        assert_eq!(
            lead("busy", STOP - 200),
            LeadWait::Waiting(WaitReason::Background)
        );
        assert_eq!(
            lead("done", STOP - 200),
            LeadWait::Waiting(WaitReason::Finished)
        );
        assert_eq!(lead("other", STOP - 40_000), LeadWait::NotWaiting);
        assert_eq!(lead("other", STOP - 60_000), LeadWait::Unknown);
        assert_eq!(lead("other", STOP - 600), LeadWait::Unknown);
        assert_eq!(lead("other", STOP + 5), LeadWait::NotWaiting);
        // The session's next call ends its restored wait.
        started.clear(holder(&mine, NATIVE), NATIVE, None, false);
        assert_eq!(started.dialog_wait("usr_me", NATIVE, STOP + 100), None);
        // The list is taken once: a Hub that then dies without saving starts knowing nothing.
        let died = Waiting::default();
        died.restore(&*store, STOP + 200);
        assert!(died.owned(&*store, &mine, STOP + 300).waits.is_empty());
        assert_eq!(
            died.lead("usr_me", "tok_a", "idle", STOP + 150, STOP + 300),
            LeadWait::Unknown
        );
        // A list from a Hub that was down too long is not believed.
        stopping.save(&*store, STOP).unwrap();
        let late = Waiting::default();
        late.restore(&*store, STOP + SAVED_MAX_AGE_SECONDS);
        assert!(
            late.owned(&*store, &mine, STOP + SAVED_MAX_AGE_SECONDS)
                .waits
                .is_empty()
        );
    }

    #[test]
    fn a_session_that_ends_after_its_turn_keeps_its_ask_until_a_call_or_a_day() {
        let (_directory, store) = question_store();
        const ASK: &str = "Shall I merge it?";
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        let shown = |now| waiting.owned(&*store, &mine, now).waits;
        // A client that ends its session after each turn: the turn's end, then the session's.
        let turn = |at| {
            let reason = WaitReason::Idle;
            waiting
                .record(&mine, "s", "s", "claude", reason, None, Some(ASK), at)
                .unwrap();
            waiting.ended("tok_a", "s");
        };
        // The person's answer (the session resumed, a start) or a new prompt in that session ends
        // it, as does the agent's next tool call. The hook route's tests show which call is which.
        for (call, all) in [("a start or a prompt", true), ("a tool call", false)] {
            turn(1_000);
            let kept = shown(1_000 + GRACE_SECONDS);
            assert_eq!(kept.len(), 1, "{call}");
            assert_eq!(
                (kept[0].reason, kept[0].asks.as_deref()),
                (WaitReason::Idle, Some(ASK))
            );
            waiting.clear("tok_a", "s", None, all);
            assert!(shown(1_000 + GRACE_SECONDS).is_empty(), "{call}");
        }
        // Unanswered, it is kept for the table's 24 hours.
        turn(2_000);
        assert_eq!(shown(2_000 + MAX_AGE_SECONDS - 1).len(), 1);
        assert!(shown(2_000 + MAX_AGE_SECONDS).is_empty());
        waiting.clear("tok_a", "s", None, true);
        // A dialog closes with its session, a stop that asks nobody is over, and so is a turn's
        // end that says nothing it asks.
        for reason in [
            WaitReason::Permission,
            WaitReason::Question,
            WaitReason::Finished,
            WaitReason::Background,
            WaitReason::Idle,
        ] {
            waiting
                .record(&mine, "s", "s", "claude", reason, None, None, 3_000)
                .unwrap();
            waiting.ended("tok_a", "s");
            assert!(!waiting.has_session("s"), "{reason:?}");
        }
    }

    #[test]
    fn a_lead_session_waits_until_its_next_call_and_background_work_for_an_hour() {
        let (_directory, store) = question_store();
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let wait = |reason, at| {
            waiting
                .record(&mine, "native", "s1", "claude", reason, None, None, at)
                .unwrap();
        };
        let lead = |session, now| waiting.lead("usr_me", "tok_a", session, 990, now);
        wait(WaitReason::Idle, 1_000);
        // The Work waits from the moment its session stops; only the notice has a grace.
        assert_eq!(lead("s1", 1_000), LeadWait::Waiting(WaitReason::Idle));
        assert!(waiting.owned(&*store, &mine, 1_000).waits.is_empty());
        // Another session, and another token's session of the same name, do not.
        assert_eq!(lead("s2", 1_100), LeadWait::NotWaiting);
        assert_eq!(
            waiting.lead("usr_me", "tok_b", "s1", 990, 1_100),
            LeadWait::NotWaiting
        );
        // Its next call ends the wait.
        waiting.clear("tok_a", "native", None, false);
        assert_eq!(lead("s1", 1_100), LeadWait::NotWaiting);
        // A stop on its own background work is nobody's wait, and nothing shows that the work
        // still runs, so it is believed only as long as a running shell call.
        wait(WaitReason::Background, 2_000);
        assert!(waiting.owned(&*store, &mine, 2_100).waits.is_empty());
        assert_eq!(
            lead("s1", 2_000 + IN_FLIGHT_CAP_SECONDS - 1),
            LeadWait::Waiting(WaitReason::Background)
        );
        assert_eq!(
            lead("s1", 2_000 + IN_FLIGHT_CAP_SECONDS),
            LeadWait::NotWaiting
        );
    }

    #[test]
    fn a_turn_that_ended_asking_nothing_is_nobodys_wait() {
        let (_directory, store) = question_store();
        // Endings modelled on real last messages. These ask, or are not known to ask nothing:
        // a question or a request to the person in the last paragraph, another language, and
        // too little to read.
        for asks in [
            "Which of the two should I merge?",
            "Both are ready. Let me know which one to merge.",
            "The next step requires your AWS sign-in.",
            "Two options: A (rebase) or B (merge). Pick one and I will go on. The tests pass for both.",
            "Say go and I will merge it. The checks are green and the diff is small.",
            "Die Migration ist fertig und die Tests laufen durch.",
            "どちらにしますか？",
            "Done.",
            "",
            "I need your API key to continue.",
            "Checkpoint recorded for wrk_1a2b. Shall I merge it?",
            "Checkpoint recorded. Shall I merge the branch now.",
            "Merged and pushed. Which branch next.",
            "Unblock the deploy and I will continue.",
            "Sag mir, was ich als Nächstes tun soll, dann will ich weitermachen.",
            "Should I open the pull request? The branch is pushed and the tests pass.",
            "The branch is pushed and the tests pass.\n\nDo you want me to open the pull request \
             now, or wait until the review of the migration is done?",
            "Both fixes are written and tested on both stores.\n\nApprove the push and I will \
             open the pull request.",
            "The deploy stopped at the health check.\n\nReply with the region and I will \
             retry it there.",
            "The unused volumes take about 31 GiB and deleting them is not reversible.\n\nTell \
             me which ones to keep.",
            "The release is ready.\n\n**Shall I tag it?**",
            "Here is the plan:\n\n- one\n- two\n\nWould you like the second step first?",
            "Both are rebased on main.\n\nWhich one should I merge first?\n\n- #12, the \
             schema fix\n- #13, the docs",
            "The cache is stale.\n\nShould I clear it with this?\n\n```\nagentscm cache \
             clear\n```",
            "Two ways to fix it:\n\n1. Retry the call\n2. Drop the cache\n\nPick one.",
        ] {
            assert!(!turn_asked_nothing(asks), "{asks}");
        }
        // These report and ask nothing, though they mention the owner, waiting, needing or being
        // blocked, or asked something in an earlier paragraph.
        for done in [
            "The Work is completed and its claim is released. The first call failed because it \
             passed a field that the schema does not have; the retry with only the outcome and \
             the summary succeeded.",
            "Both results are in: there are 2 text files in the current folder, and the first \
             one holds the word one.",
            "The command was denied from the Hub by its person. The run stopped there and has \
             not been tried again, so the file was not created.",
            "The pull request is merged into main after its checks passed. Both problems from \
             the review are fixed and both have tests. One thing is still unchecked: asking \
             three repositories at once could overspend the monthly allowance, and that is only \
             noted in the pull request.",
            "Verified so far against current main: both findings are real. The fix for the \
             first is written and its test run is in the build queue. Still to do in this \
             batch: the second finding, then two more.",
            // Only an identifier, a path or a number, and a report of a recorded state.
            "wrk_90c27ff8d0054b06218dbc90b9c083ae",
            "wrk_89d7104ec0f8a465b9349113025f86ff — blocker checkpoint recorded (generation 1, \
             state blocked, event_seq 64493).",
            "Done. Next I will run the tests.",
            "The fix, which adds a test, is merged and the checks passed.",
            "src/hub/hook.rs",
            "64493",
            // Status words that only report.
            "Pull request 412 is queued. I am waiting for CI and will merge it once the checks \
             pass.",
            "The owner approved the plan and the first pull request merged. The second one is \
             waiting on the merge queue until the first lands.",
            "Reported to the owner in the lead session. Nothing else is needed from this session \
             until the review comes back.",
            "I stopped here until the migration is approved.",
            "The branch is pushed and the checks are running; you can merge it when they pass.",
            "I am blocked on the review of the migration, so the second pull request waits.",
            "Still blocked: the release workflow has no access to the signing key. Recorded as a \
             blocker on the Work.",
            "I can’t continue on the deploy: the token has expired. The rest of the batch merged \
             and is recorded.",
            "The deploy has stopped and I need to rerun the tests before I can proceed.",
            "Unable to reproduce the failure on main; the flaky test passed ten times in a row.",
            "Decision for the reviewer is recorded: the unused volumes take about 31 GiB and \
             were left in place. Nothing has merged since noon.",
            "The fetch with ?page=2 returned the rest of the list, and all of it is recorded.",
            "Why did the first run fail? The cache held an old schema.\n\nThe cache is cleared, \
             the rerun passed and the fix is merged.",
            "Should the cache be cleared? It was, and the rerun passed.\n\nThe fix is merged \
             and recorded:\n\n- src/cache.rs\n- src/cache_test.rs",
        ] {
            assert!(turn_asked_nothing(done), "{done}");
        }
        // The client's answer for the whole message decides. Without it an uncut excerpt is the
        // whole message; a cut one may hide an ask before the cut, and no excerpt says nothing.
        let report = "The fix is merged and the tests pass for both stores.";
        let cut = format!("…{report}");
        for (answer, excerpt, want) in [
            (Some(true), None, WaitReason::Finished),
            (Some(true), Some(cut.as_str()), WaitReason::Finished),
            (Some(false), Some(report), WaitReason::Idle),
            (None, Some(report), WaitReason::Finished),
            (None, Some(cut.as_str()), WaitReason::Idle),
            (None, Some("Shall I merge it?"), WaitReason::Idle),
            (None, None, WaitReason::Idle),
        ] {
            assert_eq!(
                WaitReason::turn_end(answer, excerpt),
                want,
                "{answer:?} {excerpt:?}"
            );
        }
        let waiting = Waiting::default();
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        let record = |reason, asks, at| {
            waiting
                .record(&mine, "native", "s1", "claude", reason, None, asks, at)
                .unwrap();
            waiting.owned(&*store, &mine, 5_000).waits.len()
        };
        // The Work its session leads knows the turn ended; its person is shown no wait.
        assert_eq!(record(WaitReason::Finished, None, 1_000), 0);
        assert_eq!(
            waiting.lead("usr_me", "tok_a", "s1", 990, 5_000),
            LeadWait::Waiting(WaitReason::Finished)
        );
        // The host's idle notice a minute later says nothing, so nothing changes.
        assert_eq!(record(WaitReason::Idle, None, 1_060), 0);
        // A turn whose end asks is a wait.
        assert_eq!(record(WaitReason::Idle, Some("Which one?"), 1_100), 1);
    }

    #[test]
    fn after_a_restart_a_session_that_has_not_called_since_stays_unknown() {
        let waiting = Waiting::default();
        // A Hub that started at 5 000 never saw what a session reported before then.
        waiting.lost_before(5_000);
        assert_eq!(
            waiting.lead("usr_me", "tok_a", "s1", 4_999, 6_000),
            LeadWait::Unknown
        );
        // A session that called since would have reported its wait to this Hub.
        assert_eq!(
            waiting.lead("usr_me", "tok_a", "s1", 5_000, 6_000),
            LeadWait::NotWaiting
        );
        // A missing explicit question may still be unanswered after a day.
        assert_eq!(
            waiting.lead("usr_me", "tok_a", "s1", 4_999, 4_999 + MAX_AGE_SECONDS),
            LeadWait::Unknown
        );
        // A wait reported to this Hub is known, whatever came before.
        let mine = principal("tok_a", "usr_me", "acme/widgets");
        waiting
            .record(
                &mine,
                "native",
                "s1",
                "claude",
                WaitReason::Question,
                None,
                None,
                5_500,
            )
            .unwrap();
        assert_eq!(
            waiting.lead("usr_me", "tok_a", "s1", 4_999, 6_000),
            LeadWait::Waiting(WaitReason::Question)
        );
    }
}
