//! Native sessions that stopped to wait for their person: a permission prompt, a question, or the
//! end of a turn, as that session's own hooks reported it. Any agent can report one: the native
//! hooks of each supported agent where its host exposes the moment, and the generic hook's
//! `waiting` event for the rest. The session's next tool call, prompt or start clears it, so it
//! never outlives the wait it describes. So does the session's end, except for the end of a turn
//! that says what it asks its person: a host that ends its session after every turn still waits
//! for that person's reply ([`Waiting::ended`]).
//!
//! Short-lived like the setup observations beside it: in memory and bounded. A Hub that is
//! stopped leaves its waits in the store for the process that starts next ([`Waiting::save`]);
//! one that dies does not, and then a waiting session reads as not known rather than not
//! waiting. The wait of a session its host named belongs to its person, not to the agent token
//! that reported it (see [`holder`]). The only text kept is the short excerpt of what the wait
//! asks that the hook client sends, with secrets redacted: the end of the agent's last message,
//! its question, or, for a permission prompt, the tool input's own `description`: never the
//! command, an edit's file content or anything a command feeds a program. It is shown to that
//! session's own person alone. No prompt, other tool input or transcript is kept here, and this
//! table cannot answer a prompt. A wait at a permission prompt or a question may have a pending
//! request in `permission_answer`, which lets that person answer it from the Hub.
use std::collections::BTreeMap;
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
/// A saved list older than this is not believed: too much went unheard while the Hub was down.
const SAVED_MAX_AGE_SECONDS: i64 = 10 * 60;
/// The longest excerpt of what a wait asks that is kept, in characters.
const MAX_ASKS_CHARS: usize = 500;

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

/// Whether the end of an agent's last message may ask its person for something. A wrong yes
/// only shows a session its person need not answer; a wrong no hides a request. So a question
/// mark, any word to the person, and any word for waiting, needing or being stuck counts.
fn asks_person(text: &str) -> bool {
    const WAITING: &[&str] = &[
        "wait",
        "until",
        "need",
        "require",
        "approv",
        "confirm",
        "choose",
        "choice",
        "pick",
        "decision",
        "decide",
        "permission",
        "sign in",
        "sign-in",
        "log in",
        "login",
        "credential",
        "expired",
        "block",
        "stuck",
        "unable",
        "cannot",
        "can't",
        "proceed",
        "go ahead",
        "go-ahead",
        "say go",
        "reply",
        "respond",
        "provide",
        "let me know",
        "tell me",
        "send me",
        "please",
        "should i",
        "want me to",
        "owner",
    ];
    // A typographic apostrophe spells the same word.
    let lower = text.to_lowercase().replace('’', "'");
    text.contains(['?', '？'])
        || addresses_person(text)
        || WAITING.iter().any(|phrase| lower.contains(phrase))
}

/// Whether `text` is English prose the rule above can read: nearly all of its letters are
/// unaccented, and it uses at least two of a few words other languages do not share. Anything
/// else (another language, a bare word or two, code) is not known to ask nothing.
fn reads_as_english(text: &str) -> bool {
    const COMMON: &[&str] = &[
        "the", "and", "are", "with", "this", "that", "for", "not", "have", "from", "been", "its",
    ];
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    let plain = text.chars().filter(char::is_ascii_alphabetic).count();
    let mut common = words(text);
    common.retain(|word| COMMON.contains(&word.as_str()));
    common.sort_unstable();
    common.dedup();
    letters > 0 && plain * 10 >= letters * 9 && common.len() >= 2
}

/// Whether an agent's whole last message asked its person nothing, so its turn's end is read
/// and not answered: English prose with no sign of an ask. The hook client judges the message
/// on the person's machine and sends only this answer; the Hub judges nothing it has not all of.
pub fn turn_asked_nothing(message: &str) -> bool {
    reads_as_english(message) && !asks_person(message)
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
}

impl Row {
    fn current(&self, now: i64) -> bool {
        self.since <= now
            && now - self.since < self.reason.keep_seconds()
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

/// The waits of a Hub that stopped, as [`Waiting::save`] leaves them.
#[derive(Serialize, Deserialize)]
struct Saved {
    at: i64,
    /// The mark of [`Waiting::lost_before`].
    lost_before: i64,
    rows: Vec<SavedRow>,
}

/// One saved wait: who waits, for what kind of answer and since when. Never what it asks.
#[derive(Serialize, Deserialize)]
struct SavedRow {
    holder: String,
    native: String,
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
    Mutex<BTreeMap<(String, String), Row>>,
    AtomicI64,
    (AtomicI64, AtomicI64),
);

impl Waiting {
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
        if let Some(row) = rows.get_mut(&(kept_by.to_owned(), native.to_owned())) {
            row.noticed = true;
        }
        Ok(())
    }

    /// Whether the host said a dialog is on screen for the current wait of `native` at one.
    pub(super) fn noticed(&self, holder: &str, native: &str, now: i64) -> bool {
        self.0.lock().is_ok_and(|rows| {
            rows.get(&(holder.to_owned(), native.to_owned()))
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
        let key = (kept_by.to_owned(), native.to_owned());
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
            },
        );
        Ok(())
    }

    /// Leaves the current waits in the store for the Hub process that starts next. What a wait
    /// asks is not saved: that text is kept in memory only.
    pub(super) fn save(&self, store: &dyn Store, now: i64) -> Result<()> {
        let saved = {
            let rows = self.0.lock().map_err(|_| HubError::Busy)?;
            let rows = rows
                .iter()
                .filter(|(_, row)| row.current(now))
                .map(|((holder, native), row)| SavedRow {
                    holder: holder.clone(),
                    native: native.clone(),
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
            }
        };
        let text = serde_json::to_string(&saved).unwrap_or_default();
        store.set_setting(SAVED_KEY, &text, "hub")
    }

    /// Takes the waits the previous Hub process left when it stopped. The list is taken once,
    /// so a Hub that later dies without saving does not start from an old one, and it is
    /// believed only while fresh. Without a list, the waits reported to the previous process are
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
            .filter(|saved| saved.at <= now && now - saved.at < SAVED_MAX_AGE_SECONDS);
        let (Some(saved), Ok(mut rows)) = (saved, self.0.lock()) else {
            self.lost_before(now);
            return;
        };
        self.lost_before(saved.lost_before);
        // A session whose last call is older than a running call is believed was not at work.
        let unheard = (saved.at - IN_FLIGHT_CAP_SECONDS, now);
        self.2.0.store(unheard.0, Ordering::Relaxed);
        self.2.1.store(unheard.1, Ordering::Relaxed);
        for row in saved.rows.into_iter().take(MAX_ROWS) {
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
            };
            if wait.current(now) {
                rows.insert((row.holder, row.native), wait);
            }
        }
    }

    /// Says that a wait may be missing for any session whose last call was before `at`: the
    /// waits a Hub dropped for room, or the ones reported to a process that left no list.
    pub(super) fn lost_before(&self, at: i64) {
        self.1.fetch_max(at, Ordering::Relaxed);
    }

    /// Whether anyone's session `native` waits: lets a hook skip authenticating a clear.
    pub(super) fn has_session(&self, native: &str) -> bool {
        self.0
            .lock()
            .is_ok_and(|rows| rows.keys().any(|(_, session)| session == native))
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
        let key = (holder.to_owned(), native.to_owned());
        if rows
            .get(&key)
            .is_some_and(|row| all || row.asker.is_none() || row.asker.as_deref() == by)
        {
            rows.remove(&key);
        }
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
        let key = (holder.to_owned(), native.to_owned());
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
        let row = rows.get(&(holder.to_owned(), native.to_owned()))?;
        let dialog = matches!(row.reason, WaitReason::Permission | WaitReason::Question);
        (row.current(now) && dialog).then(|| (row.since, row.asker.clone(), row.reason))
    }

    /// The waits of `viewer`'s own sessions in the repository `viewer` reads that have lasted at
    /// least [`GRACE_SECONDS`], oldest first.
    pub(super) fn owned(&self, viewer: &Principal, now: i64) -> Result<Vec<WaitingSession>> {
        let rows = self.0.lock().map_err(|_| HubError::Busy)?;
        let mut out = rows
            .iter()
            .filter(|(_, row)| {
                row.current(now)
                    && !row.reason.asks_nobody()
                    && now - row.since >= GRACE_SECONDS
                    && row.principal.owner.id == viewer.owner.id
                    && same_repository(&row.principal, viewer)
            })
            .map(|((holder, native), row)| WaitingSession {
                key: wait_key(holder, native),
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
        Ok(out)
    }

    /// Whether the session `session` waits, for the Work it leads for `owner` under agent token
    /// `token`, which last called at `last_call`. A wait kept under its person is that session's
    /// whichever of the person's tokens leads the Work: the Work takes a renewed token only at
    /// the session's next Work call, and the wait at its next report. Unlike `owned` it has no grace: the Work waits from the moment
    /// its session stops, and only the notice to its person is held back. With no wait on record
    /// the answer is unknown while one may be missing; after 24 hours no wait would still be
    /// believed, so then the session is not waiting.
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
        let wait = rows.iter().find(|(_, row)| {
            let leads = if row.person {
                row.principal.owner.id == owner
            } else {
                row.principal.token_id == token
            };
            leads && row.session == session && row.current(now)
        });
        let unheard = self.2.0.load(Ordering::Relaxed)..self.2.1.load(Ordering::Relaxed);
        let missing = last_call < self.1.load(Ordering::Relaxed) || unheard.contains(&last_call);
        match wait {
            Some((_, row)) => LeadWait::Waiting(row.reason),
            None if missing && now - last_call < MAX_AGE_SECONDS => LeadWait::Unknown,
            None => LeadWait::NotWaiting,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn a_wait_shows_after_the_grace_only_to_its_owner_in_its_repository_until_cleared() {
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
                .owned(&mine, 1_000 + GRACE_SECONDS - 1)
                .unwrap()
                .is_empty()
        );
        let shown = waiting.owned(&mine, 1_000 + GRACE_SECONDS).unwrap();
        assert_eq!(shown.len(), 1);
        assert_eq!(
            (shown[0].reason, shown[0].since, shown[0].session.as_str()),
            (WaitReason::Permission, 1_000, "s1")
        );
        assert!(!shown[0].key.contains("s1") && !shown[0].key.contains("tok_a"));
        // A repeat of the same wait keeps its start; a different wait starts again.
        wait(&mine, "s1", WaitReason::Permission, 1_100);
        assert_eq!(waiting.owned(&mine, 1_200).unwrap()[0].since, 1_000);
        wait(&mine, "s1", WaitReason::Idle, 1_150);
        assert!(waiting.owned(&mine, 1_200).unwrap().is_empty());
        assert_eq!(
            waiting.owned(&mine, 1_210).unwrap()[0].reason,
            WaitReason::Idle
        );
        // Only the session's own token clears it.
        assert!(waiting.has_session("s1"));
        waiting.clear("tok_b", "s1", None, true);
        assert_eq!(waiting.owned(&mine, 1_210).unwrap().len(), 1);
        waiting.clear("tok_a", "s1", Some("any-subagent"), false);
        assert!(!waiting.has_session("s1"));
        assert!(waiting.owned(&mine, 1_210).unwrap().is_empty());
    }

    #[test]
    fn a_repeat_that_says_nothing_keeps_what_the_wait_asked() {
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
            waiting.owned(&mine, 2_000).unwrap()[0].asks.clone()
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
            waiting.owned(&mine, 2_000).unwrap()[0].asks.as_deref(),
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
        let shown = waiting.owned(&mine, 1_100).unwrap();
        assert!(shown.len() == 1 && shown[0].subagent);
        waiting.clear("tok_a", "s", Some("sub-a"), false);
        assert!(waiting.owned(&mine, 1_100).unwrap().is_empty());
        ask();
        waiting.clear("tok_a", "s", None, true);
        assert!(waiting.owned(&mine, 1_100).unwrap().is_empty());
    }

    #[test]
    fn old_expired_read_only_and_excess_waits_are_dropped() {
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
        assert_eq!(waiting.owned(&mine, MAX_AGE_SECONDS - 1).unwrap().len(), 1);
        assert!(waiting.owned(&mine, MAX_AGE_SECONDS).unwrap().is_empty());
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
        assert!(waiting.owned(&mine, 2_000).unwrap().is_empty());
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
        let shown = waiting.owned(&mine, 5_000).unwrap();
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
        let shown = waiting.owned(&new, 3_000).unwrap();
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
        assert_eq!(waiting.owned(&new, 1_900).unwrap().len(), 1);
        assert!(!waiting.owned(&new, 1_900).unwrap()[0].person);
        assert!(waiting.owned(&new, 2_000).unwrap().is_empty());
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
            stopping.owned(&mine, STOP + 100).unwrap(),
            started.owned(&mine, STOP + 100).unwrap(),
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
        assert!(died.owned(&mine, STOP + 300).unwrap().is_empty());
        assert_eq!(
            died.lead("usr_me", "tok_a", "idle", STOP + 150, STOP + 300),
            LeadWait::Unknown
        );
        // A list from a Hub that was down too long is not believed.
        stopping.save(&*store, STOP).unwrap();
        let late = Waiting::default();
        late.restore(&*store, STOP + SAVED_MAX_AGE_SECONDS);
        assert!(
            late.owned(&mine, STOP + SAVED_MAX_AGE_SECONDS)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn a_session_that_ends_after_its_turn_keeps_its_ask_until_a_call_or_a_day() {
        const ASK: &str = "Shall I merge it?";
        let waiting = Waiting::default();
        let mut mine = principal("tok_a", "usr_me", "acme/widgets");
        mine.expires_at = i64::MAX;
        let shown = |now| waiting.owned(&mine, now).unwrap();
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
        assert!(waiting.owned(&mine, 1_000).unwrap().is_empty());
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
        assert!(waiting.owned(&mine, 2_100).unwrap().is_empty());
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
        // Endings modelled on real last messages. These ask, or are not known to ask nothing:
        // a question, words to the person, words for waiting or needing, an ask that sits
        // before a long report, another language, and too little to read.
        for asks in [
            "Which of the two should I merge?",
            "Both are ready. Let me know which one to merge.",
            "The next step requires your AWS sign-in.",
            "I stopped here until the migration is approved.",
            "The branch is pushed and the checks are running; you can merge it when they pass.",
            "Two options: A (rebase) or B (merge). Pick one and I will go on. The tests pass for both.",
            "Say go and I will merge it. The checks are green and the diff is small.",
            "I am stuck: the build fails with a linker error and the cause is not clear.",
            "I can’t continue. The token has expired and the deploy stopped for that reason.",
            "Decision for the reviewer: the unused volumes take about 31 GiB, and deleting them \
             is not reversible. Status of the open pull requests: 12 conflicts with main, and 13 \
             and 14 have had no pushes since this morning. Nothing has merged since noon.",
            "Die Migration ist fertig und die Tests laufen durch.",
            "どちらにしますか？",
            "Done.",
        ] {
            assert!(!turn_asked_nothing(asks), "{asks}");
        }
        // These report and ask nothing.
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
            waiting.owned(&mine, 5_000).unwrap().len()
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
    fn after_a_restart_a_session_that_has_not_called_since_reads_as_unknown_for_a_day() {
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
        // No wait is believed for longer than a day, so by then its absence means quiet.
        assert_eq!(
            waiting.lead("usr_me", "tok_a", "s1", 4_999, 4_999 + MAX_AGE_SECONDS),
            LeadWait::NotWaiting
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
