//! Contested-edit refusals and overlap notices, and what the Hub observed each one lead to.
//!
//! A row is written in the touch that refused an edit (`refusal`) or reported an overlap without
//! refusing (`notice`). Later Hub calls settle it: the same session edited the file again while
//! the hold that caused it still kept the file (`pushed_through`), or the session asked for or was
//! granted the file (`handed_off`). An unsettled row is read against its hold: while the hold
//! keeps the file it is `pending`; once the hold no longer keeps it the session `went_elsewhere`;
//! when the hold's record is gone the outcome is `unknown`. Only what the Hub saw is counted.
use serde::{Deserialize, Serialize};

/// Rows older than this are deleted when a new row is written.
pub const RETAIN_SECONDS: i64 = 30 * 86_400;
/// Rows kept per repository; the oldest go first.
pub const MAX_ROWS: i64 = 20_000;
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

/// The hold that caused a row, as it stands when the row is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Hold {
    /// The hold still keeps the file.
    Live,
    /// The hold exists but no longer keeps the file (released, expired or handed over).
    Ended,
    /// The hold's record is gone, so how it ended is not known.
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Pending,
    PushedThrough,
    HandedOff,
    WentElsewhere,
    Unknown,
}

/// One stored row as read for a metrics window.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct Observed {
    pub kind: Kind,
    /// The stored settlement: `pending`, `pushed_through` or `handed_off`.
    pub settled: String,
    pub observed_at: i64,
    pub hold: Hold,
}

impl Observed {
    pub fn outcome(&self) -> Outcome {
        match self.settled.as_str() {
            "pushed_through" => Outcome::PushedThrough,
            "handed_off" => Outcome::HandedOff,
            _ => match self.hold {
                Hold::Live => Outcome::Pending,
                Hold::Ended => Outcome::WentElsewhere,
                Hold::Missing => Outcome::Unknown,
            },
        }
    }
}

/// A stored row as a store reads it: `live` when the hold still keeps the file, `known` when the
/// hold's record exists. None for a kind this version does not know.
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

/// Whether a window's rows are all there: no more than `MAX_READ` matched, none of the window
/// is older than the retention, and the repository is not at its row cap (which prunes by count).
pub fn complete(matched: usize, since: i64, now: i64, at_cap: bool) -> bool {
    matched <= MAX_READ && since >= now - RETAIN_SECONDS && !at_cap
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct OutcomeCounts {
    pub observed: usize,
    pub pushed_through: usize,
    pub handed_off: usize,
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
            Outcome::HandedOff => self.handed_off += 1,
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

    /// Settled away from the file: asked for or got it, or left it while the hold stood.
    pub fn resolved(&self) -> usize {
        [&self.refusals, &self.notices]
            .iter()
            .map(|c| c.handed_off + c.went_elsewhere)
            .sum()
    }

    /// Edited the file again while the hold that caused the row still kept it.
    pub fn dismissed(&self) -> usize {
        self.refusals.pushed_through + self.notices.pushed_through
    }

    /// Not settled yet, or its outcome cannot be known.
    pub fn unresolved(&self) -> usize {
        [&self.refusals, &self.notices]
            .iter()
            .map(|c| c.pending + c.unknown)
            .sum()
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
            row(Kind::Notice, "pending", Hold::Live).outcome(),
            Outcome::Pending
        );
        assert_eq!(
            row(Kind::Notice, "pending", Hold::Ended).outcome(),
            Outcome::WentElsewhere
        );
        assert_eq!(
            row(Kind::Refusal, "pending", Hold::Missing).outcome(),
            Outcome::Unknown
        );
    }

    #[test]
    fn counts_split_by_kind_and_feed_issue_totals() {
        let mut metrics = ContestedMetrics::default();
        assert_eq!(metrics.availability, "none_observed");
        metrics.add(&[
            row(Kind::Refusal, "pushed_through", Hold::Live),
            row(Kind::Refusal, "handed_off", Hold::Ended),
            row(Kind::Refusal, "pending", Hold::Ended),
            row(Kind::Notice, "pending", Hold::Live),
            row(Kind::Notice, "pending", Hold::Missing),
        ]);
        assert_eq!(metrics.availability, "observed");
        assert_eq!(metrics.refusals.observed, 3);
        assert_eq!(metrics.notices.observed, 2);
        assert_eq!(metrics.dismissed(), 1);
        assert_eq!(metrics.resolved(), 2);
        assert_eq!(metrics.unresolved(), 2);
    }
}
