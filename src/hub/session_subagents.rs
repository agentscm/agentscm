//! The subagents of native sessions, as each session's own hooks reported them: one started,
//! made a call, asked its person something, or stopped. A host that names the subagent on its
//! hook calls (Claude Code sends `agent_id` and `agent_type`) fills this; other hosts leave it
//! empty.
//!
//! Short-lived like the waits beside it: in memory, bounded, and lost on a Hub restart, after
//! which a session's subagents read as not known rather than none. The only text kept is the
//! subagent's type and the short description its session gave when it started it, as the hook
//! client excerpted it with secrets redacted. Both are shown to that session's own person alone;
//! anyone else who may see the session sees only that it has subagents and their state.
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use super::advice::sanitize;
use super::session::SESSION_TTL_SECONDS;
use super::store::Principal;
use super::work_lifecycle::IN_FLIGHT_CAP_SECONDS;

const MAX_SESSIONS: usize = 1024;
const MAX_PER_SESSION: usize = 32;
const MAX_TYPE_CHARS: usize = 64;
/// The longest description of a subagent's task that is kept, in characters.
const MAX_DOES_CHARS: usize = 120;
/// A subagent that stopped stays listed this long, so its session's page shows that it ended.
const ENDED_KEEP_SECONDS: i64 = SESSION_TTL_SECONDS;
/// A running subagent that made no call for this long went away without stopping. Hook calls
/// arrive at least once a minute while it uses tools, and nothing longer than a running shell
/// call is believed without one.
const UNSEEN_KEEP_SECONDS: i64 = IN_FLIGHT_CAP_SECONDS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubagentState {
    Running,
    /// It asked its person something and its whole session waits on the answer.
    Waiting,
    Ended,
}

impl SubagentState {
    /// Stable for other surfaces to match on.
    pub fn code(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Ended => "ended",
        }
    }
}

/// One subagent as a reader may see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Subagent {
    pub state: SubagentState,
    pub started_at: i64,
    pub seen_at: i64,
    /// Its type (`Explore`, a custom agent's name); for its session's own person only.
    pub kind: Option<String>,
    /// What its session started it to do; third-party text for that person only.
    pub does: Option<String>,
}

struct Row {
    id: String,
    /// Empty until a call names its type; a row without one is not listed.
    kind: String,
    started: i64,
    seen: i64,
    ended: Option<i64>,
    waiting: bool,
    does: Option<String>,
}

impl Row {
    fn current(&self, now: i64) -> bool {
        match self.ended {
            Some(at) => now - at < ENDED_KEEP_SECONDS,
            None => now - self.seen < UNSEEN_KEEP_SECONDS,
        }
    }
}

struct Session {
    owner: String,
    rows: Vec<Row>,
}

/// Keyed by agent token id and the session ID the hook reported. The second field is the Hub's
/// start, before which this table saw nothing.
#[derive(Default)]
pub struct Subagents(Mutex<BTreeMap<(String, String), Session>>, AtomicI64);

impl Subagents {
    /// Says that this Hub started at `at`, so for a while a session's subagents may be missing.
    pub(super) fn started(&self, at: i64) {
        self.1.fetch_max(at, Ordering::Relaxed);
    }

    /// The subagent's row in `native`, made when `kind` names its type and it has none yet.
    fn with(
        &self,
        principal: &Principal,
        native: &str,
        id: &str,
        kind: Option<&str>,
        now: i64,
        change: impl FnOnce(&mut Row),
    ) {
        let Ok(mut sessions) = self.0.lock() else {
            return;
        };
        sessions.retain(|_, session| {
            session.rows.retain(|row| row.current(now));
            !session.rows.is_empty()
        });
        let key = (principal.token_id.clone(), native.to_owned());
        let kind = kind
            .map(|kind| sanitize(kind.trim(), MAX_TYPE_CHARS))
            .filter(|kind| !kind.is_empty());
        if !sessions.contains_key(&key) {
            if kind.is_none() {
                return;
            }
            // shortcut: a full table drops the session seen longest ago, whose subagents then
            // read as none; say unknown for it if Hubs ever hold this many sessions with subagents.
            if sessions.len() >= MAX_SESSIONS
                && let Some(oldest) = sessions
                    .iter()
                    .min_by_key(|(_, session)| session.rows.iter().map(|row| row.seen).max())
                    .map(|(key, _)| key.clone())
            {
                sessions.remove(&oldest);
            }
        }
        let session = sessions.entry(key).or_insert_with(|| Session {
            owner: principal.owner.id.clone(),
            rows: Vec::new(),
        });
        if !session.rows.iter().any(|row| row.id == id) {
            let Some(kind) = kind.clone() else {
                return;
            };
            if session.rows.len() >= MAX_PER_SESSION {
                // An ended one gives way first, then the one seen longest ago.
                let drop = session
                    .rows
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, row)| (row.ended.is_none(), row.seen))
                    .map(|(at, _)| at);
                if let Some(at) = drop {
                    session.rows.remove(at);
                }
            }
            session.rows.push(Row {
                id: id.to_owned(),
                kind,
                started: now,
                seen: now,
                ended: None,
                waiting: false,
                does: None,
            });
        }
        if let Some(row) = session.rows.iter_mut().find(|row| row.id == id) {
            if let Some(kind) = kind {
                row.kind = kind;
            }
            change(row);
        }
    }

    /// A hook call by the subagent `id` of `native`: it runs, and `waiting` says whether this
    /// call is its ask of its person. A subagent that stopped and was resumed runs again.
    pub(super) fn seen(
        &self,
        principal: &Principal,
        native: &str,
        id: &str,
        kind: Option<&str>,
        waiting: bool,
        now: i64,
    ) {
        self.with(principal, native, id, kind, now, |row| {
            row.seen = now;
            row.ended = None;
            row.waiting = waiting;
        });
    }

    /// The subagent `id` of `native` stopped. One this table never saw start is not added: the
    /// host also reports the stop of agents it runs for itself.
    pub(super) fn ended(&self, principal: &Principal, native: &str, id: &str, now: i64) {
        self.with(principal, native, id, None, now, |row| {
            row.ended = Some(now);
            row.waiting = false;
        });
    }

    /// What `native` started its subagent `id` to do, from the call that started it. `running`
    /// says that call returned while the subagent still runs, so a start this table has not
    /// seen yet is added; a finished one is only described.
    pub(super) fn described(
        &self,
        principal: &Principal,
        native: &str,
        id: &str,
        kind: Option<&str>,
        does: &str,
        running: bool,
        now: i64,
    ) {
        let does = Some(sanitize(does.trim(), MAX_DOES_CHARS)).filter(|does| !does.is_empty());
        self.with(
            principal,
            native,
            id,
            kind.filter(|_| running),
            now,
            |row| {
                row.does = does;
            },
        );
    }

    /// The person answered `native`: none of its subagents waits any longer.
    pub(super) fn answered(&self, token: &str, native: &str) {
        if let Ok(mut sessions) = self.0.lock()
            && let Some(session) = sessions.get_mut(&(token.to_owned(), native.to_owned()))
        {
            session.rows.iter_mut().for_each(|row| row.waiting = false);
        }
    }

    /// The session `native` ended, and its subagents with it.
    pub(super) fn forget(&self, token: &str, native: &str) {
        if let Ok(mut sessions) = self.0.lock() {
            sessions.remove(&(token.to_owned(), native.to_owned()));
        }
    }

    /// The subagents of `native`, oldest first, with their text only for `viewer` when that is
    /// the session's own person. `None` when they are not known: this Hub holds none and started
    /// too recently to have heard from one that was already running.
    pub(super) fn of(
        &self,
        token: &str,
        native: &str,
        viewer: &str,
        now: i64,
    ) -> Option<Vec<Subagent>> {
        let sessions = self.0.lock().ok()?;
        let session = sessions.get(&(token.to_owned(), native.to_owned()));
        let own = session.is_some_and(|session| session.owner == viewer);
        let mut out = session
            .into_iter()
            .flat_map(|session| &session.rows)
            .filter(|row| row.current(now) && !row.kind.is_empty())
            .map(|row| Subagent {
                state: match (row.ended, row.waiting) {
                    (Some(_), _) => SubagentState::Ended,
                    (None, true) => SubagentState::Waiting,
                    (None, false) => SubagentState::Running,
                },
                started_at: row.started,
                seen_at: row.ended.unwrap_or(row.seen),
                kind: own.then(|| row.kind.clone()),
                does: row.does.clone().filter(|_| own),
            })
            .collect::<Vec<_>>();
        if out.is_empty() && now - self.1.load(Ordering::Relaxed) < UNSEEN_KEEP_SECONDS {
            return None;
        }
        out.sort_by_key(|subagent| subagent.started_at);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::store::{ForgeIdentity, User};

    fn principal(token: &str, owner: &str) -> Principal {
        Principal {
            token_id: token.into(),
            name: format!("{token}-laptop"),
            repository: "acme/widgets".into(),
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

    const LATER: i64 = 100_000;

    #[test]
    fn a_subagent_runs_from_its_first_call_waits_on_its_ask_and_ends_when_it_stops() {
        let roster = Subagents::default();
        let mine = principal("tok_a", "usr_me");
        let of = |now| roster.of("tok_a", "s", "usr_me", now).unwrap();
        assert!(of(LATER).is_empty());
        // A call that names no type starts nothing: the type is what a person is shown.
        roster.seen(&mine, "s", "a1", None, false, LATER);
        assert!(of(LATER).is_empty());
        roster.seen(&mine, "s", "a1", Some(" Explore "), false, LATER);
        roster.seen(&mine, "s", "a1", None, false, LATER + 30);
        let listed = of(LATER + 30);
        assert_eq!(listed.len(), 1);
        assert_eq!(
            (
                listed[0].state,
                listed[0].started_at,
                listed[0].seen_at,
                listed[0].kind.as_deref(),
                listed[0].does.as_deref()
            ),
            (
                SubagentState::Running,
                LATER,
                LATER + 30,
                Some("Explore"),
                None
            )
        );
        // Its ask makes it wait until it calls again or its person answers the session.
        roster.seen(&mine, "s", "a1", None, true, LATER + 40);
        assert_eq!(of(LATER + 40)[0].state, SubagentState::Waiting);
        roster.answered("tok_a", "s");
        assert_eq!(of(LATER + 40)[0].state, SubagentState::Running);
        roster.seen(&mine, "s", "a1", None, true, LATER + 50);
        roster.seen(&mine, "s", "a1", None, false, LATER + 60);
        assert_eq!(of(LATER + 60)[0].state, SubagentState::Running);
        // A stop ends it; it stays listed for a while, then goes.
        roster.ended(&mine, "s", "a1", LATER + 70);
        let ended = of(LATER + 80);
        assert_eq!(
            (ended[0].state, ended[0].seen_at),
            (SubagentState::Ended, LATER + 70)
        );
        assert!(of(LATER + 70 + ENDED_KEEP_SECONDS).is_empty());
        // A resumed subagent runs again, listed after the older one that ended.
        roster.seen(&mine, "s", "a2", Some("Plan"), false, LATER + 100);
        roster.ended(&mine, "s", "a2", LATER + 110);
        roster.seen(&mine, "s", "a2", Some("Plan"), false, LATER + 120);
        let states = |now| of(now).iter().map(|s| s.state).collect::<Vec<_>>();
        assert_eq!(
            states(LATER + 120),
            [SubagentState::Ended, SubagentState::Running]
        );
        // The stop of an agent that never started here adds nothing.
        roster.ended(&mine, "s", "internal", LATER + 130);
        assert_eq!(states(LATER + 130).len(), 2);
        // The session's end takes its subagents with it.
        roster.forget("tok_a", "s");
        assert!(of(LATER + 130).is_empty());
    }

    #[test]
    fn a_description_reaches_only_the_sessions_own_person() {
        let roster = Subagents::default();
        let mine = principal("tok_a", "usr_me");
        roster.seen(&mine, "s", "a1", Some("Explore"), false, LATER);
        // The start call returned while a1 runs: its description is added to its row.
        roster.described(
            &mine,
            "s",
            "a1",
            Some("Explore"),
            " Find \"session\" keys ",
            true,
            LATER + 1,
        );
        // A background start whose own first call has not arrived yet is added by it.
        roster.described(
            &mine,
            "s",
            "a2",
            Some("Plan"),
            "Plan the read",
            true,
            LATER + 2,
        );
        // A subagent that already finished and was never seen is not brought back.
        roster.described(&mine, "s", "a3", Some("Plan"), "Old", false, LATER + 3);
        let own = roster.of("tok_a", "s", "usr_me", LATER + 3).unwrap();
        assert_eq!(
            own.iter()
                .map(|s| (s.kind.as_deref(), s.does.as_deref()))
                .collect::<Vec<_>>(),
            [
                (Some("Explore"), Some("Find 'session' keys")),
                (Some("Plan"), Some("Plan the read"))
            ]
        );
        // Anyone else sees that they exist and their state, and no text.
        let theirs = roster.of("tok_a", "s", "usr_them", LATER + 3).unwrap();
        assert_eq!(theirs.len(), 2);
        assert!(
            theirs
                .iter()
                .all(|s| s.kind.is_none() && s.does.is_none() && s.state == SubagentState::Running)
        );
        // Another token's session of the same name is another session.
        assert!(
            roster
                .of("tok_b", "s", "usr_me", LATER + 3)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn unseen_and_excess_subagents_go_and_a_restart_reads_as_unknown() {
        let roster = Subagents::default();
        let mine = principal("tok_a", "usr_me");
        roster.seen(&mine, "s", "quiet", Some("Explore"), false, LATER);
        let of = |now| roster.of("tok_a", "s", "usr_me", now).unwrap();
        assert_eq!(of(LATER + UNSEEN_KEEP_SECONDS - 1).len(), 1);
        assert!(of(LATER + UNSEEN_KEEP_SECONDS).is_empty());
        // One session cannot hold more than its share: ended ones give way first.
        let start = LATER + UNSEEN_KEEP_SECONDS;
        roster.seen(&mine, "s", "done", Some("Explore"), false, start);
        roster.ended(&mine, "s", "done", start + 1);
        for n in 0..MAX_PER_SESSION {
            let at = start + 2 + i64::try_from(n).unwrap();
            roster.seen(&mine, "s", &format!("a{n}"), Some("Explore"), false, at);
        }
        let listed = of(start + 100);
        assert_eq!(listed.len(), MAX_PER_SESSION);
        assert!(listed.iter().all(|s| s.state == SubagentState::Running));
        // A Hub that just started has not heard from subagents that were already running.
        let restarted = Subagents::default();
        restarted.started(LATER);
        assert_eq!(restarted.of("tok_a", "s", "usr_me", LATER + 60), None);
        restarted.seen(&mine, "s", "a1", Some("Explore"), false, LATER + 61);
        assert_eq!(
            restarted
                .of("tok_a", "s", "usr_me", LATER + 62)
                .unwrap()
                .len(),
            1
        );
        // By the time an unseen one would have been dropped anyway, none means none.
        assert_eq!(
            restarted.of("tok_a", "other", "usr_me", LATER + UNSEEN_KEEP_SECONDS),
            Some(vec![])
        );
    }
}
