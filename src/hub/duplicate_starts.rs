//! A lead start that named a forge issue whose unfinished Work another person leads, and what the
//! Hub saw the asking person do next.
//!
//! One row is written the first time a session's lead `agentscm_start` names such an issue and is
//! not admitted: the claim is held, the preflight is not clear, or a takeover was refused. A
//! helper or reviewer join, a start by `work_id`, a takeover that went through, a start on Work
//! offered with a handover checkpoint and a start on one person's own Work (by any of their
//! agents) write nothing. Reads (overview, search, preflight,
//! planned issues) are not recorded.
//!
//! Outcomes are worked out when a row is read, from what the Hub already keeps. `proceeded`: within
//! [`WINDOW_SECONDS`] of the start the asking person joined or led the holder's Work, started Work
//! of their own linked to the issue, or drew a contested-edit row on a file the holder kept.
//! `pending`: none of these yet and the window is still open. `stood_down`: none of these within
//! the window. `unknown`: the holder's Work record is gone. `stood_down` is inferred from what the
//! Hub did not see, so it is never counted as resolved, and none of these feed the issue totals.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::Result;
use super::store::Event;
use super::work::{ForgeWorkRef, WorkItem, WorkRole, WorkStart, WorkState, WorkWho};

pub use super::refusal_outcomes::{MAX_READ, MAX_ROWS_PER_TOKEN, RETAIN_SECONDS, complete};

/// How long after a start the asking person's next step is looked for.
pub const WINDOW_SECONDS: i64 = 86_400;

/// The holder side of a start worth recording.
pub(super) struct Seen {
    pub holder_work: String,
    pub holder_owner: String,
    pub holder_login: String,
    pub source: ForgeWorkRef,
}

/// None unless a lead start named a forge source that resolved to unfinished Work, not offered
/// for handover, whose owner and lead owner both differ from the caller's owner: one person's two
/// agents are not someone else.
pub(super) fn seen(who: &WorkWho, start: &WorkStart, subject: Option<&WorkItem>) -> Option<Seen> {
    let item = subject?;
    let source = start.source.clone()?;
    if who.read_only
        || start.role != WorkRole::Lead
        || start.work_id.is_some()
        || matches!(item.state, WorkState::Finished | WorkState::Handover)
        || item.owner == who.owner.id
    {
        return None;
    }
    let lead = item
        .participants
        .iter()
        .find(|p| p.role == WorkRole::Lead && p.token == item.lead_token);
    let (holder_owner, holder_login) = lead.map_or_else(
        || (item.owner.clone(), item.owner_login.clone()),
        |p| (p.owner.clone(), p.login.clone()),
    );
    (holder_owner != who.owner.id).then(|| Seen {
        holder_work: item.id.clone(),
        holder_owner,
        holder_login,
        source,
    })
}

/// The `duplicate_start` activity event; its subject is the asking token.
pub(super) fn event(token: &str, seen: &Seen, answer: Value, now: i64) -> Result<Event> {
    let mut event = Event::new(
        "duplicate_start",
        token,
        json!({
            "token": token,
            "issue": seen.source.issue,
            "source": seen.source,
            "work": seen.holder_work,
            "holder": {"login": seen.holder_login},
            "answer": answer,
        }),
    )?;
    event.at = now;
    Ok(event)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Proceeded,
    StoodDown,
    Pending,
    Unknown,
}

/// A row's outcome at `now`: `holder_known` when the holder's Work record exists, `proceeded`
/// when the asking person's next step was seen within the window.
pub fn outcome(observed_at: i64, holder_known: bool, proceeded: bool, now: i64) -> Outcome {
    if proceeded {
        Outcome::Proceeded
    } else if !holder_known {
        Outcome::Unknown
    } else if now < observed_at + WINDOW_SECONDS {
        Outcome::Pending
    } else {
        Outcome::StoodDown
    }
}

/// Counts for the window. `availability` is `observed` when rows were read, `none_observed` when
/// the window had none, and `not_scoped_to_work` for a single-Work query (rows name the holder's
/// Work, not the asker's).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct DuplicateStartMetrics {
    pub availability: String,
    pub observed: usize,
    pub proceeded: usize,
    pub stood_down: usize,
    pub pending: usize,
    pub unknown: usize,
    /// More rows matched than were read, or rows were pruned inside the window.
    pub partial: bool,
}

impl Default for DuplicateStartMetrics {
    fn default() -> Self {
        Self {
            availability: "none_observed".into(),
            observed: 0,
            proceeded: 0,
            stood_down: 0,
            pending: 0,
            unknown: 0,
            partial: false,
        }
    }
}

impl DuplicateStartMetrics {
    pub fn add(&mut self, rows: &[Outcome]) {
        for row in rows {
            self.observed += 1;
            match row {
                Outcome::Proceeded => self.proceeded += 1,
                Outcome::StoodDown => self.stood_down += 1,
                Outcome::Pending => self.pending += 1,
                Outcome::Unknown => self.unknown += 1,
            }
        }
        if self.observed > 0 {
            self.availability = "observed".into();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seen_next_step_wins_then_a_missing_holder_then_the_window() {
        let t = 1_000;
        assert_eq!(outcome(t, false, true, t + 1), Outcome::Proceeded);
        assert_eq!(outcome(t, false, false, t + 1), Outcome::Unknown);
        assert_eq!(
            outcome(t, true, false, t + WINDOW_SECONDS - 1),
            Outcome::Pending
        );
        assert_eq!(
            outcome(t, true, false, t + WINDOW_SECONDS),
            Outcome::StoodDown
        );
    }

    #[test]
    fn counts_add_up_to_observed() {
        let mut metrics = DuplicateStartMetrics::default();
        assert_eq!(metrics.availability, "none_observed");
        metrics.add(&[
            Outcome::Proceeded,
            Outcome::StoodDown,
            Outcome::Pending,
            Outcome::Unknown,
            Outcome::Pending,
        ]);
        assert_eq!(metrics.availability, "observed");
        assert_eq!(metrics.observed, 5);
        assert_eq!(
            (
                metrics.proceeded,
                metrics.stood_down,
                metrics.pending,
                metrics.unknown
            ),
            (1, 1, 2, 1)
        );
    }
}
