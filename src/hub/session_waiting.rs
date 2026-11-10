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
//! and this table cannot answer a prompt. A permission wait may have a pending request in
//! `permission_answer`, which lets that person allow or deny it once from the Hub.
use std::collections::BTreeMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use super::onboarding::same_repository;
use super::store::Principal;
use super::{HubError, Result};

/// A wait shorter than this is someone already at the keyboard, so it is never shown.
pub const GRACE_SECONDS: i64 = 60;
/// A wait this old belongs to a session that went away without ending.
const MAX_AGE_SECONDS: i64 = 24 * 60 * 60;
const MAX_ROWS: usize = 1024;
const MAX_TOKEN_ROWS: usize = 32;
/// The longest excerpt of what a wait asks that is kept, in characters.
const MAX_ASKS_CHARS: usize = 500;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WaitReason {
    /// The agent asked to use a tool and its host is showing the permission prompt.
    Permission,
    /// The agent asked the person a question: a choice, or a form an MCP server opened.
    Question,
    /// The agent finished its turn and waits for the person's next message.
    Idle,
}

impl WaitReason {
    /// Stable for other surfaces to match on.
    pub fn code(self) -> &'static str {
        match self {
            Self::Permission => "permission",
            Self::Question => "question",
            Self::Idle => "idle",
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
        }
    }
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
        self.since <= now && now - self.since < MAX_AGE_SECONDS && self.principal.expires_at > now
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
#[derive(Default)]
pub struct Waiting(Mutex<BTreeMap<(String, String), Row>>);

impl Waiting {
    /// Records that `native` (whose Hub session name is `session`) waits for its person, asked by
    /// the subagent `asker` when the host names one, with `asks` saying what for when the hook
    /// sent it. A repeat of the same wait keeps its start and takes the newer `asks`.
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
        if !rows.contains_key(&key) {
            let owned = rows.keys().filter(|(token, _)| *token == key.0);
            if owned.count() >= MAX_TOKEN_ROWS
                && let Some(oldest) = rows
                    .iter()
                    .filter(|((token, _), _)| *token == key.0)
                    .min_by_key(|(_, row)| row.since)
                    .map(|(key, _)| key.clone())
            {
                rows.remove(&oldest);
            }
            if rows.len() >= MAX_ROWS {
                return Err(HubError::Busy);
            }
        }
        let since = match rows.get(&key) {
            Some(row) if row.reason == reason => row.since,
            _ => now,
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
                    .filter(|text| !text.is_empty()),
            },
        );
        Ok(())
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

    /// The start and asking subagent of the current permission wait of `native`, or `None` when
    /// it is gone, replaced by another kind of wait, or too old. A pending answer is valid only
    /// while this still returns what it was made for.
    pub(super) fn permission_wait(
        &self,
        token: &str,
        native: &str,
        now: i64,
    ) -> Option<(i64, Option<String>)> {
        let rows = self.0.lock().ok()?;
        let row = rows.get(&(token.to_owned(), native.to_owned()))?;
        (row.current(now) && row.reason == WaitReason::Permission)
            .then(|| (row.since, row.asker.clone()))
    }

    /// The waits of `viewer`'s own sessions in the repository `viewer` reads that have lasted at
    /// least [`GRACE_SECONDS`], oldest first.
    pub(super) fn owned(&self, viewer: &Principal, now: i64) -> Result<Vec<WaitingSession>> {
        let rows = self.0.lock().map_err(|_| HubError::Busy)?;
        let mut out = rows
            .iter()
            .filter(|(_, row)| {
                row.current(now)
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
        // One token cannot hold more than its share; its oldest wait gives way.
        for n in 0..=MAX_TOKEN_ROWS {
            let native = format!("s{n}");
            let at = 1_000 + i64::try_from(n).unwrap();
            waiting
                .record(
                    &mine,
                    &native,
                    &native,
                    "cursor",
                    WaitReason::Question,
                    Some("x"),
                    None,
                    at,
                )
                .unwrap();
        }
        let shown = waiting.owned(&mine, 5_000).unwrap();
        assert_eq!(shown.len(), MAX_TOKEN_ROWS);
        assert!(shown.iter().all(|w| w.session != "s0" && w.subagent));
    }
}
