//! The hook client's own measure of its Hub calls: how long each took, and whether it timed out
//! or failed open. A call that timed out never reached the Hub, so the counts wait in a small
//! file per agent session and travel with the next call (`x-agentscm-hook-report`); once a call
//! is answered they start again from that call. No extra request is ever made for them.
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentscm::hub::hook_overhead::ClientReport;
use sha2::{Digest, Sha256};

/// The Hub ignores a report claiming more calls than this; past it the counts start again.
const MAX_CALLS: u64 = 10_000;
const MAX_CALL_MS: u64 = 600_000;

/// How one measured call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// The Hub answered, so the report this call carried arrived with it.
    Answered,
    TimedOut,
    FailedOpen,
}

/// The counts waiting for one agent session, and where they are kept.
pub struct Pending {
    path: Option<PathBuf>,
    report: ClientReport,
}

impl Pending {
    /// The waiting counts for this agent session; empty when there are none or no state dir.
    pub fn load(config: Option<&Path>, agent: &str, session: Option<&str>) -> Self {
        let path = config.zip(session).map(|(config, session)| {
            let key = Sha256::digest(format!("{agent}\0{session}"));
            config.join("hook-report").join(format!("{key:x}.json"))
        });
        let report = path
            .as_deref()
            .and_then(|path| std::fs::read(path).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self { path, report }
    }

    /// The header value to send, when there is something to report.
    pub fn header(&self) -> Option<String> {
        (self.report.calls > 0)
            .then(|| serde_json::to_string(&self.report).ok())
            .flatten()
    }

    /// Adds this call and keeps what has not been delivered yet. Best effort: a lost write
    /// loses counts and never affects the hook's answer.
    // shortcut: concurrent hooks of one session can overwrite each other's counts; a lock
    // file would fix it if the counts ever need to be exact.
    pub fn settle(self, took: Duration, sent: Sent) {
        let Some(path) = self.path else { return };
        let ms = u64::try_from(took.as_millis())
            .unwrap_or(MAX_CALL_MS)
            .min(MAX_CALL_MS);
        let this = ClientReport {
            calls: 1,
            ms_total: ms,
            ms_max: ms,
            timeouts: u64::from(sent == Sent::TimedOut),
            failed_open: u64::from(sent == Sent::FailedOpen),
        };
        let next = match sent {
            Sent::Answered => this,
            _ => add(self.report, this),
        };
        if let Some(parent) = path.parent()
            && std::fs::create_dir_all(parent).is_ok()
            && let Ok(json) = serde_json::to_vec(&next)
        {
            let _ = std::fs::write(path, json);
        }
    }
}

fn add(a: ClientReport, b: ClientReport) -> ClientReport {
    if a.calls + b.calls > MAX_CALLS {
        return b;
    }
    ClientReport {
        calls: a.calls + b.calls,
        ms_total: a.ms_total + b.ms_total,
        ms_max: a.ms_max.max(b.ms_max),
        timeouts: a.timeouts + b.timeouts,
        failed_open: a.failed_open + b.failed_open,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unanswered_calls_wait_for_the_next_answered_one_and_then_start_again() {
        let dir = tempfile::tempdir().unwrap();
        let load = || Pending::load(Some(dir.path()), "claude", Some("s1"));
        assert_eq!(load().header(), None);
        load().settle(Duration::from_millis(5000), Sent::TimedOut);
        load().settle(Duration::from_millis(40), Sent::FailedOpen);
        let waiting: ClientReport = serde_json::from_str(&load().header().unwrap()).unwrap();
        assert_eq!(
            waiting,
            ClientReport {
                calls: 2,
                ms_total: 5040,
                ms_max: 5000,
                timeouts: 1,
                failed_open: 1
            }
        );
        load().settle(Duration::from_millis(30), Sent::Answered);
        let next: ClientReport = serde_json::from_str(&load().header().unwrap()).unwrap();
        assert_eq!((next.calls, next.ms_total, next.timeouts), (1, 30, 0));
        // Another session of the same agent keeps its own counts.
        assert_eq!(
            Pending::load(Some(dir.path()), "claude", Some("s2")).header(),
            None
        );
        // Without a session or a state directory nothing is kept.
        Pending::load(None, "claude", Some("s1")).settle(Duration::ZERO, Sent::TimedOut);
        Pending::load(Some(dir.path()), "claude", None).settle(Duration::ZERO, Sent::TimedOut);
    }
}
