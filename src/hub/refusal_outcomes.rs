//! Contested-edit refusals and overlap notices, and what the Hub observed each one lead to.
//!
//! A `refusal` row is written in the touch that refused an edit. Later Hub calls settle it: the
//! same session edited the file again while its holder still kept it (`pushed_through`), the
//! session asked for the file (`asked`), or the holder granted it (`handed_off`). An unsettled
//! row is read against the holder: while any hold of the same holder token keeps the file it is
//! `pending`; once none does, the session `went_elsewhere`; when the hold's record is gone the
//! outcome is `unknown`. A hold renewed past its cap under a new id still counts as the holder's.
//! The refused session's own hold never does: two sessions of one token are two agents.
//!
//! A `notice` row is written when the Hub reported an overlap without refusing. The call that drew
//! it always took the file: its hook edit went ahead, or it claimed the file over MCP. So it is
//! written `proceeded`. That is not a dismissal of anything, so it feeds no issue total.
//!
//! Only what the Hub saw is counted, and what it cannot tell is never guessed into a favourable
//! class: `went_elsewhere` (a session that ended, or whose later hooks failed open, looks the
//! same) and `asked` (the holder may have declined) are neither resolved; only a grant is.
use serde::{Deserialize, Serialize};

/// Rows older than this are deleted when a new row is written.
pub const RETAIN_SECONDS: i64 = 30 * 86_400;
/// Rows kept per token; the oldest go first. Session IDs come from the client, so a cap per
/// repository would let one token's fresh sessions push out every other token's rows.
pub const MAX_ROWS_PER_TOKEN: i64 = 1_000;
/// At most this many rows are read for one metrics window; past it the counts are partial.
pub const MAX_READ: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Refusal,
    Notice,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Refusal => "refusal",
            Self::Notice => "notice",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "refusal" => Some(Self::Refusal),
            "notice" => Some(Self::Notice),
            _ => None,
        }
    }
}

/// The holder of a row's file, as it stands when the row is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    /// A hold of the same holder still keeps the file.
    Live,
    /// The holder no longer keeps the file (released, expired or handed over).
    Ended,
    /// The hold's record is gone, so how it ended is not known.
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pending,
    PushedThrough,
    Asked,
    HandedOff,
    Proceeded,
    WentElsewhere,
    Unknown,
}

/// One stored row as read for a metrics window.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Observed {
    pub kind: Kind,
    /// The stored settlement: `pending`, `pushed_through`, `asked`, `handed_off` or `proceeded`.
    pub settled: String,
    pub observed_at: i64,
    pub hold: Hold,
}

impl Observed {
    pub fn outcome(&self) -> Outcome {
        match self.settled.as_str() {
            "pushed_through" => Outcome::PushedThrough,
            "asked" => Outcome::Asked,
            "handed_off" => Outcome::HandedOff,
            "proceeded" => Outcome::Proceeded,
            "pending" => match self.hold {
                Hold::Live => Outcome::Pending,
                Hold::Ended => Outcome::WentElsewhere,
                Hold::Missing => Outcome::Unknown,
            },
            _ => Outcome::Unknown,
        }
    }
}

/// A stored row as a store reads it: `live` when a hold of the same holder still keeps the file,
/// `known` when the hold's record exists. None for a kind this version does not know.
pub fn observed(
    kind: &str,
    settled: String,
    observed_at: i64,
    live: bool,
    known: bool,
) -> Option<Observed> {
    let hold = match (live, known) {
        (true, _) => Hold::Live,
        (false, true) => Hold::Ended,
        (false, false) => Hold::Missing,
    };
    Some(Observed {
        kind: Kind::parse(kind)?,
        settled,
        observed_at,
        hold,
    })
}

/// Whether a window's rows are all there: no more than `MAX_READ` matched, none of the window is
/// older than the retention, and no token pruned by count rows that fell inside the window.
pub fn complete(matched: usize, since: i64, now: i64, pruned_in_window: bool) -> bool {
    matched <= MAX_READ && since >= now - RETAIN_SECONDS && !pruned_in_window
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct OutcomeCounts {
    pub observed: usize,
    pub pushed_through: usize,
    pub asked: usize,
    pub handed_off: usize,
    pub proceeded: usize,
    pub went_elsewhere: usize,
    pub pending: usize,
    pub unknown: usize,
}

impl OutcomeCounts {
    fn add(&mut self, outcome: Outcome) {
        self.observed += 1;
        match outcome {
            Outcome::Pending => self.pending += 1,
            Outcome::PushedThrough => self.pushed_through += 1,
            Outcome::Asked => self.asked += 1,
            Outcome::HandedOff => self.handed_off += 1,
            Outcome::Proceeded => self.proceeded += 1,
            Outcome::WentElsewhere => self.went_elsewhere += 1,
            Outcome::Unknown => self.unknown += 1,
        }
    }
}

/// Counts for the window. `availability` is `observed` when rows were read, `none_observed` when
/// the window had none, and `not_scoped_to_work` for a single-Work query (rows name no Work).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ContestedMetrics {
    pub availability: String,
    pub refusals: OutcomeCounts,
    pub notices: OutcomeCounts,
    /// More rows matched than were read, or rows were pruned by count inside the window.
    pub partial: bool,
}

impl Default for ContestedMetrics {
    fn default() -> Self {
        Self {
            availability: "none_observed".into(),
            refusals: OutcomeCounts::default(),
            notices: OutcomeCounts::default(),
            partial: false,
        }
    }
}

impl ContestedMetrics {
    pub fn add(&mut self, rows: &[Observed]) {
        for row in rows {
            match row.kind {
                Kind::Refusal => self.refusals.add(row.outcome()),
                Kind::Notice => self.notices.add(row.outcome()),
            }
        }
        if self.refusals.observed + self.notices.observed > 0 {
            self.availability = "observed".into();
        }
    }

    /// Refusals settled by the holder granting the file.
    pub fn resolved(&self) -> usize {
        self.refusals.handed_off
    }

    /// Refusals whose holder ended with no further edit or request seen: inferred, so kept apart
    /// from resolved and unresolved.
    pub fn went_elsewhere(&self) -> usize {
        self.refusals.went_elsewhere
    }

    /// Refusals the session overrode: it edited the file again while the holder still kept it.
    pub fn dismissed(&self) -> usize {
        self.refusals.pushed_through
    }

    /// Refusals not settled by a grant: still pending, asked for without a grant yet, or whose
    /// outcome cannot be known.
    pub fn unresolved(&self) -> usize {
        self.refusals.pending + self.refusals.asked + self.refusals.unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: Kind, settled: &str, hold: Hold) -> Observed {
        Observed {
            kind,
            settled: settled.into(),
            observed_at: 1,
            hold,
        }
    }

    #[test]
    fn a_settlement_wins_over_the_hold_and_an_unsettled_row_follows_its_hold() {
        assert_eq!(
            row(Kind::Refusal, "pushed_through", Hold::Ended).outcome(),
            Outcome::PushedThrough
        );
        assert_eq!(
            row(Kind::Refusal, "handed_off", Hold::Missing).outcome(),
            Outcome::HandedOff
        );
        assert_eq!(
            row(Kind::Refusal, "asked", Hold::Ended).outcome(),
            Outcome::Asked
        );
        assert_eq!(
            row(Kind::Notice, "proceeded", Hold::Ended).outcome(),
            Outcome::Proceeded
        );
        assert_eq!(
            row(Kind::Refusal, "pending", Hold::Live).outcome(),
            Outcome::Pending
        );
        assert_eq!(
            row(Kind::Refusal, "pending", Hold::Ended).outcome(),
            Outcome::WentElsewhere
        );
        assert_eq!(
            row(Kind::Refusal, "pending", Hold::Missing).outcome(),
            Outcome::Unknown
        );
        // A settlement this version does not know is never guessed into a class.
        assert_eq!(
            row(Kind::Refusal, "later", Hold::Live).outcome(),
            Outcome::Unknown
        );
    }

    #[test]
    fn only_refusals_feed_issue_totals_and_only_a_grant_resolves() {
        let mut metrics = ContestedMetrics::default();
        assert_eq!(metrics.availability, "none_observed");
        metrics.add(&[
            row(Kind::Refusal, "pushed_through", Hold::Live),
            row(Kind::Refusal, "handed_off", Hold::Ended),
            row(Kind::Refusal, "asked", Hold::Ended),
            row(Kind::Refusal, "pending", Hold::Ended),
            row(Kind::Refusal, "pending", Hold::Live),
            row(Kind::Notice, "proceeded", Hold::Live),
            row(Kind::Notice, "proceeded", Hold::Ended),
        ]);
        assert_eq!(metrics.availability, "observed");
        assert_eq!(
            (metrics.refusals.observed, metrics.notices.observed),
            (5, 2)
        );
        assert_eq!(metrics.notices.proceeded, 2);
        // A notice is not a dismissal: the agent was told and went ahead.
        assert_eq!(metrics.dismissed(), 1);
        assert_eq!(metrics.resolved(), 1);
        // Asked without a grant stays unresolved; went elsewhere is apart from both.
        assert_eq!(metrics.unresolved(), 2);
        assert_eq!(metrics.went_elsewhere(), 1);
    }
}
