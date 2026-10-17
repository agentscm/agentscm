//! Handoffs: an agent whose edit was refused asks the session holding the file to hand it over;
//! the holder answers once. The backends run the SQL; the shared decisions are here.
use serde_json::json;

use super::advice::ascii;
use super::intent::{Intent, display_all, spec_key};
use super::session::{HOLD_CAP_SECONDS, Held, SessionCall, never_hold, same_agent};
use super::store::Event;
use super::{HubError, Result};

const MAX_NOTE_CHARS: usize = 200;
/// Open requests per requesting session, and per held file.
pub(super) const MAX_OPEN: usize = 3;
/// Handoff lines one touch delivers per side; the rest wait for the next touch.
pub(super) const PER_TOUCH: i64 = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asked {
    Requested {
        holder: Intent,
    },
    /// Nobody else holds the file now: a retry of the edit passes.
    NotHeld,
    /// The asking session holds it.
    Yours,
    AlreadyAsked {
        holder: Intent,
    },
    TooMany,
}

/// The requester is `(agent, login)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Answered {
    Granted {
        requester: (String, String),
    },
    Declined {
        requester: (String, String),
    },
    NoRequest,
    /// The holder no longer holds the file; the request lapsed.
    NotHeld,
}

/// A handoff line for the touched session: `kind` is `request` (to the holder), `grant` or
/// `decline` (to the requester); `agent` and `login` are the other side's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffNote {
    pub kind: &'static str,
    pub path: String,
    pub agent: String,
    pub login: String,
    pub note: Option<String>,
    /// A decline's reason.
    pub reason: Option<String>,
    /// The touched session's name.
    pub session: String,
}

/// An open request as the answering backend reads it.
pub(super) struct Request {
    pub id: String,
    pub requester_token: String,
    pub requester_session: String,
    pub holder_session: String,
    /// The requester's token name and owner login.
    pub agent: String,
    pub login: String,
}

/// The index key of a file a session can hold.
pub(super) fn file_key(path: &str) -> Result<String> {
    let key = spec_key(path)?;
    if key.ends_with('/') || never_hold(&key) {
        return Err(HubError::Invalid(
            "path must be one file that sessions hold".into(),
        ));
    }
    Ok(key)
}

pub(super) fn clean_note(note: Option<&str>) -> Option<String> {
    note.map(|note| ascii(note, MAX_NOTE_CHARS))
        .filter(|note| !note.is_empty())
}

/// Requests created at or before this are lapsed.
pub(super) fn cutoff(now: i64) -> i64 {
    now - HOLD_CAP_SECONDS
}

pub(super) enum Verdict {
    Yours,
    NotHeld,
    AlreadyAsked(Intent),
    TooMany,
    Request(Intent),
}

/// `holders` are the live intents holding the file (the asker's own included); `token` and
/// `via` (JSON) are the asker's; `own` is the asker's intent; `asked` are the intents this
/// session has open requests to for the file; `open` is the session's open requests in all;
/// `queued` are the intents all requesters have open requests to for the file.
pub(super) fn judge(
    mut holders: Vec<Intent>,
    (token, via): (&str, &str),
    own: Option<&str>,
    (asked, open, queued): (&[String], usize, &[String]),
) -> Verdict {
    let mine = holders.iter().any(|intent| Some(intent.id.as_str()) == own);
    // Only other session intents hand files over, and not to the same agent: an asker that
    // announced the file itself (agentscm_start with paths) can still ask its other holder.
    let via: Vec<String> = serde_json::from_str(via).unwrap_or_default();
    holders.retain(|intent| {
        Some(intent.id.as_str()) != own
            && !intent.via.is_empty()
            && !same_agent(token, &via, intent)
    });
    holders.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
    let Some(holder) = holders.into_iter().next() else {
        return if mine {
            Verdict::Yours
        } else {
            Verdict::NotHeld
        };
    };
    if asked.contains(&holder.id) {
        Verdict::AlreadyAsked(holder)
    } else if open >= MAX_OPEN || queued.iter().filter(|id| **id == holder.id).count() >= MAX_OPEN {
        Verdict::TooMany
    } else {
        Verdict::Request(holder)
    }
}

/// Whether the holder's intent still holds `key`.
pub(super) fn still_holds(held: &Held, key: &str) -> bool {
    held.keys.iter().any(|held| held == key)
}

/// The display specs left in the holder's intent once `key` is handed over.
pub(super) fn remaining(held: &Held, key: &str) -> Vec<String> {
    display_all(held.keys.iter().filter(|held| *held != key))
}

/// The requester's side of a grant: the file joins its session as a touch would add it, under a
/// connection mode the session already has so no mode is gained.
pub(super) fn grant_call(session: &str, key: &str, via_json: Option<&str>) -> SessionCall {
    let via = via_json
        .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
        .and_then(|via| via.into_iter().next());
    SessionCall {
        session: Some(session.to_owned()),
        via: via.unwrap_or_else(|| "mcp".into()),
        task: None,
        paths: vec![key.to_owned()],
        deny: false,
        report: false,
    }
}

/// The note kind of a decided request.
pub(super) fn answer_kind(state: &str) -> &'static str {
    if state == "granted" {
        "grant"
    } else {
        "decline"
    }
}

impl Event {
    pub(super) fn handoff_request(
        token: &str,
        request: &str,
        session: &str,
        path: &str,
        note: Option<&str>,
        holder: &Intent,
        auto: bool,
    ) -> Result<Self> {
        let data = json!({"request": request, "paths": [path], "note": note, "session": session,
            "auto": auto, "to": {"agent": holder.agent, "login": holder.owner.login}});
        Self::new("handoff_request", token, data)
    }

    /// `kind` is `handoff_grant` or `handoff_decline`; the subject is the holder's token.
    pub(super) fn handoff_answer(
        kind: &'static str,
        holder: &str,
        session: &str,
        path: &str,
        request: &Request,
        reason: Option<&str>,
    ) -> Result<Self> {
        let data = json!({"request": request.id, "paths": [path], "session": session,
            "from": {"agent": request.agent, "login": request.login}, "reason": reason});
        Self::new(kind, holder, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::advice::test_intent;

    fn holder(id: &str, created_at: i64, via: &[&str]) -> Intent {
        let mut intent = test_intent(id, "t", &["a.rs"]);
        intent.created_at = created_at;
        intent.via = via.iter().map(|via| (*via).to_owned()).collect();
        intent
    }

    #[test]
    fn only_files_that_sessions_hold_can_be_asked_for() {
        assert_eq!(file_key("src/a.rs").unwrap(), "src/a.rs");
        for bad in ["src/**", "Cargo.lock", "../x", "", "dist/a.js"] {
            assert!(file_key(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn notes_are_filtered_and_bounded() {
        assert_eq!(clean_note(None), None);
        assert_eq!(clean_note(Some(" \"`\n")), None);
        assert_eq!(
            clean_note(Some("need \"it\"\nnow")).as_deref(),
            Some("need it now")
        );
        let long = clean_note(Some(&"word ".repeat(100))).unwrap();
        assert!(long.len() <= 200 && long.ends_with("..."), "{long}");
    }

    #[test]
    fn the_verdict_follows_who_holds_and_what_was_asked() {
        let (a, b) = (
            holder("int_a", 5, &["mcp"]),
            holder("int_b", 3, &["hook:x"]),
        );
        let verdict = |holders: Vec<Intent>, own: Option<&str>, asked: &[&str], open| {
            let asked: Vec<String> = asked.iter().map(|id| (*id).to_owned()).collect();
            judge(
                holders,
                ("tok_x", r#"["hook:x"]"#),
                own,
                (&asked, open, &[]),
            )
        };
        // The oldest session holder is asked.
        let both = || vec![a.clone(), b.clone()];
        assert!(matches!(verdict(both(), None, &[], 0), Verdict::Request(h) if h.id == "int_b"));
        // Holding it too does not stop asking the other holder; holding it alone is Yours.
        assert!(
            matches!(verdict(both(), Some("int_a"), &[], 0), Verdict::Request(h) if h.id == "int_b")
        );
        assert!(matches!(
            verdict(vec![a.clone()], Some("int_a"), &[], 0),
            Verdict::Yours
        ));
        // An intent announced through the intent API has no session to ask.
        let rest = holder("int_c", 1, &[]);
        assert!(matches!(
            verdict(vec![rest], None, &[], 0),
            Verdict::NotHeld
        ));
        assert!(matches!(verdict(vec![], None, &[], 0), Verdict::NotHeld));
        assert!(
            matches!(verdict(both(), None, &["int_b"], 3), Verdict::AlreadyAsked(h) if h.id == "int_b")
        );
        assert!(matches!(
            verdict(both(), None, &["int_a"], 3),
            Verdict::TooMany
        ));
        assert!(matches!(verdict(both(), None, &[], 2), Verdict::Request(_)));
        // Three open requests to the holder, from anyone, are the cap.
        let queued = vec!["int_b".to_owned(); 3];
        assert!(matches!(
            judge(both(), ("tok_x", "[]"), None, (&[], 0, &queued)),
            Verdict::TooMany
        ));
        // The same agent (same token, an MCP-only side) is not a holder to ask.
        let mine = holder("int_d", 1, &["mcp"]);
        let token = mine.token.clone();
        assert!(matches!(
            judge(vec![mine], (&token, r#"["hook:x"]"#), None, (&[], 0, &[])),
            Verdict::NotHeld
        ));
    }

    #[test]
    fn a_grant_keeps_the_sessions_mode_and_the_holders_other_files() {
        let held = Held {
            id: "int_a".into(),
            keys: vec!["a.rs".into(), "d/".into(), "z.rs".into()],
            created_at: 0,
        };
        assert!(still_holds(&held, "a.rs") && !still_holds(&held, "b.rs"));
        assert_eq!(remaining(&held, "a.rs"), ["d/**", "z.rs"]);
        let call = grant_call("s1", "a.rs", Some(r#"["hook:claude"]"#));
        assert_eq!((call.via.as_str(), call.deny), ("hook:claude", false));
        assert_eq!(grant_call("s1", "a.rs", None).via, "mcp");
        assert_eq!(grant_call("s1", "a.rs", Some("[]")).via, "mcp");
    }
}
