//! Native sessions that stopped to wait for their person: a permission prompt, a question, or the
//! end of a turn, as that session's own hooks reported it. Any agent can report one: the native
//! hooks of each supported agent where its host exposes the moment, and the generic hook's
//! `waiting` event for the rest. The session's next tool call, prompt, start or end clears it, so
//! it never outlives the wait it describes.
//!
//! Short-lived like the setup observations beside it: in memory, bounded, and lost on a Hub
//! restart, after which a waiting session reads as not known rather than not waiting. The only
//! text kept is the short excerpt of what the wait asks that the hook client sends (the end of the
//! agent's last message, its question, or what a permission prompt is for, with secrets redacted),
//! shown to that session's own person alone. No prompt, tool input or transcript is kept here,
//! and this table cannot answer a prompt. A wait at a permission prompt or a question may have a
//! pending request in `permission_answer`, which lets that person answer it from the Hub.
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use sha2::{Digest, Sha256};

use super::onboarding::same_repository;
use super::store::Principal;
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
    /// Stable per agent token and native session; neither a secret nor the session ID itself.
    pub key: String,
    pub reason: WaitReason,
    pub since: i64,
    /// The display name of the agent token.
    pub name: String,
    /// The hook's agent (`claude`, `gemini`, ...), for display.
    pub agent: String,
    /// The agent token id, to link the Work the session leads. Never shown.
    pub token: String,
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
}

impl Row {
    fn current(&self, now: i64) -> bool {
        self.since <= now
            && now - self.since < self.reason.keep_seconds()
            && self.principal.expires_at > now
    }
}

/// The item key of a wait: stable per agent token and native session, and neither a secret nor
/// the session ID itself.
pub(super) fn wait_key(token: &str, native: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(token, native)).unwrap_or_default())
    )
}

/// Keyed by agent token id and the session ID the hook reported.
/// The second field is the time before which a session's last call leaves its wait unknown: the
/// Hub's start, or just after the start of the newest wait it dropped for room.
#[derive(Default)]
pub struct Waiting(Mutex<BTreeMap<(String, String), Row>>, AtomicI64);

impl Waiting {
    /// Records that `native` (whose Hub session name is `session`) waits for its person, asked by
    /// the subagent `asker` when the host names one, with `asks` saying what for when the hook
    /// sent it. A repeat of the same wait keeps its start and takes the newer `asks`; a repeat by
    /// the same asker that says nothing (the host's notice after its own prompt) keeps the older,
    /// and leaves a question a question.
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
        let key = (principal.token_id.clone(), native.to_owned());
        // The host's notice that a dialog is on screen arrives as a permission prompt that says
        // nothing, also when the dialog is a question. That is the wait already on record.
        // shortcut: any permission report that says nothing is taken for the notice. A real
        // prompt follows a tool call, which ends the question first; pass the notice down from
        // the hook route if a prompt ever follows a question with no call between.
        if reason == WaitReason::Permission
            && asks.is_none()
            && rows.get(&key).is_some_and(|row| {
                row.reason == WaitReason::Question && row.asker.as_deref() == asker
            })
        {
            return Ok(());
        }
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
        let (since, said) = match rows.get(&key) {
            Some(row) if row.reason == reason => (
                row.since,
                row.asks.clone().filter(|_| row.asker.as_deref() == asker),
            ),
            _ => (now, None),
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
            },
        );
        Ok(())
    }

    /// Says that a wait may be missing for any session whose last call was before `at`. The Hub
    /// says so for its own start, since the waits reported to the previous process are gone.
    pub(super) fn lost_before(&self, at: i64) {
        self.1.fetch_max(at, Ordering::Relaxed);
    }

    /// Whether any token's session `native` waits: lets a hook skip authenticating a clear.
    pub(super) fn has_session(&self, native: &str) -> bool {
        self.0
            .lock()
            .is_ok_and(|rows| rows.keys().any(|(_, session)| session == native))
    }

    /// Ends the wait of `native` after a call from it: `all` for a prompt, start or end of the
    /// session; otherwise a tool call by `by` (a subagent, or `None` for the lead), which ends
    /// only a wait that subagent asked, or one whose asker the host did not name. Another
    /// subagent working on in parallel does not end it.
    pub(super) fn clear(&self, token: &str, native: &str, by: Option<&str>, all: bool) {
        let Ok(mut rows) = self.0.lock() else {
            return;
        };
        let key = (token.to_owned(), native.to_owned());
        if rows
            .get(&key)
            .is_some_and(|row| all || row.asker.is_none() || row.asker.as_deref() == by)
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
        token: &str,
        native: &str,
        now: i64,
    ) -> Option<(i64, Option<String>, WaitReason)> {
        let rows = self.0.lock().ok()?;
        let row = rows.get(&(token.to_owned(), native.to_owned()))?;
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
            .map(|((token, native), row)| WaitingSession {
                key: wait_key(token, native),
                reason: row.reason,
                since: row.since,
                name: row.principal.name.clone(),
                agent: row.agent.clone(),
                token: token.clone(),
                session: row.session.clone(),
                subagent: row.asker.is_some(),
                asks: row.asks.clone(),
            })
            .collect::<Vec<_>>();
        out.sort_by(|a, b| (a.since, &a.key).cmp(&(b.since, &b.key)));
        Ok(out)
    }

    /// Whether the session `session` of agent token `token` waits, for the Work it leads, which
    /// last called at `last_call`. Unlike `owned` it has no grace: the Work waits from the moment
    /// its session stops, and only the notice to its person is held back. With no wait on record
    /// the answer is unknown while one may be missing; after 24 hours no wait would still be
    /// believed, so then the session is not waiting.
    pub(super) fn lead(&self, token: &str, session: &str, last_call: i64, now: i64) -> LeadWait {
        let Ok(rows) = self.0.lock() else {
            return LeadWait::Unknown;
        };
        let wait = rows
            .iter()
            .find(|((owner, _), row)| owner == token && row.session == session && row.current(now));
        match wait {
            Some((_, row)) => LeadWait::Waiting(row.reason),
            None if last_call < self.1.load(Ordering::Relaxed)
                && now - last_call < MAX_AGE_SECONDS =>
            {
                LeadWait::Unknown
            }
            None => LeadWait::NotWaiting,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::store::{ForgeIdentity, User};

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
        // The notice names a permission prompt and says nothing: same wait, same start.
        assert_eq!(record(WaitReason::Permission, None, 1_006), question);
        assert_eq!(
            waiting.owned(&mine, 2_000).unwrap()[0].asks.as_deref(),
            Some("Which color?")
        );
        // A permission prompt that says what it is for is a new wait, and the end of a turn is
        // no dialog at all.
        assert_eq!(
            record(WaitReason::Permission, Some("Push the branch"), 1_010),
            Some((1_010, None, WaitReason::Permission))
        );
        assert_eq!(record(WaitReason::Idle, None, 1_020), None);
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
            waiting.lead("tok_a", "idle0", 1_000, 5_000),
            LeadWait::Unknown
        );
        assert_eq!(
            waiting.lead("tok_a", "s2", 1_002, 5_000),
            LeadWait::Waiting(WaitReason::Question)
        );
        record(&mine, "new1", WaitReason::Permission, 2_001).unwrap();
        assert_eq!(
            waiting.lead("tok_b", "idle1", 1_001, 5_000),
            LeadWait::Unknown
        );
        // With only questions and prompts left, a new wait is refused and none is dropped.
        assert!(matches!(
            record(&mine, "new2", WaitReason::Question, 2_002),
            Err(HubError::Busy)
        ));
        assert_eq!(
            waiting.lead("tok_a", "s2", 1_002, 5_000),
            LeadWait::Waiting(WaitReason::Question)
        );
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
        let lead = |session, now| waiting.lead("tok_a", session, 990, now);
        wait(WaitReason::Idle, 1_000);
        // The Work waits from the moment its session stops; only the notice has a grace.
        assert_eq!(lead("s1", 1_000), LeadWait::Waiting(WaitReason::Idle));
        assert!(waiting.owned(&mine, 1_000).unwrap().is_empty());
        // Another session, and another token's session of the same name, do not.
        assert_eq!(lead("s2", 1_100), LeadWait::NotWaiting);
        assert_eq!(
            waiting.lead("tok_b", "s1", 990, 1_100),
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
            waiting.lead("tok_a", "s1", 990, 5_000),
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
        assert_eq!(waiting.lead("tok_a", "s1", 4_999, 6_000), LeadWait::Unknown);
        // A session that called since would have reported its wait to this Hub.
        assert_eq!(
            waiting.lead("tok_a", "s1", 5_000, 6_000),
            LeadWait::NotWaiting
        );
        // No wait is believed for longer than a day, so by then its absence means quiet.
        assert_eq!(
            waiting.lead("tok_a", "s1", 4_999, 4_999 + MAX_AGE_SECONDS),
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
            waiting.lead("tok_a", "s1", 4_999, 6_000),
            LeadWait::Waiting(WaitReason::Question)
        );
    }
}
