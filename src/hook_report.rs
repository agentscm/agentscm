//! The hook client's own measure of its Hub calls: how long each took, and whether it timed out
//! or failed open. A call that timed out never reached the Hub, so the counts wait in small files
//! per agent session and travel with the next call (`x-agentscm-hook-report`); once a call is
//! answered they start again from that call. No extra request is ever made for them.
//!
//! Each measurement is its own file, and a call takes waiting files by renaming them first, so two
//! hooks of one session running at once never send the same counts twice or overwrite each other.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentscm::hub::hook_overhead::ClientReport;
use sha2::{Digest, Sha256};

/// The Hub ignores a report claiming more calls than this; past it the counts start again.
const MAX_CALLS: u64 = 10_000;
const MAX_CALL_MS: u64 = 600_000;
/// Waiting files one call takes; any more wait for the next call.
const MAX_FILES: usize = 100;

/// How one measured call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// The Hub answered, so the report this call carried arrived with it.
    Answered,
    TimedOut,
    FailedOpen,
}

/// The counts this call took for its agent session, and where that session's files are kept.
pub struct Pending {
    dir: Option<PathBuf>,
    report: ClientReport,
}

/// A name no other call uses: this process, the time, and a per-process counter.
fn unique(suffix: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{nanos}-{}-{n}{suffix}", std::process::id())
}

impl Pending {
    /// Takes the waiting counts for this agent session; empty when there are none, no session
    /// or no state directory. A file another call renamed first is left to that call.
    pub fn load(config: Option<&Path>, agent: &str, session: Option<&str>) -> Self {
        let dir = config.zip(session).map(|(config, session)| {
            let key = Sha256::digest(format!("{agent}\0{session}"));
            config.join("hook-report").join(format!("{key:x}"))
        });
        let mut report = ClientReport::default();
        if let Some(entries) = dir.as_deref().and_then(|dir| std::fs::read_dir(dir).ok()) {
            for entry in entries.flatten().take(MAX_FILES) {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "json") {
                    continue;
                }
                let taken = path.with_extension(unique(".taken"));
                if std::fs::rename(&path, &taken).is_err() {
                    continue;
                }
                if let Some(counts) = std::fs::read(&taken)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
                {
                    report = add(report, counts);
                }
                let _ = std::fs::remove_file(&taken);
            }
        }
        Self { dir, report }
    }

    /// The header value to send, when there is something to report.
    pub fn header(&self) -> Option<String> {
        (self.report.calls > 0)
            .then(|| serde_json::to_string(&self.report).ok())
            .flatten()
    }

    /// Leaves this call's counts, plus the taken ones when they were not delivered, in a new
    /// file for a later call. Best effort: a lost write loses counts and never affects the hook.
    pub fn settle(self, took: Duration, sent: Sent) {
        let Some(dir) = self.dir else { return };
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
        if std::fs::create_dir_all(&dir).is_ok()
            && let Ok(json) = serde_json::to_vec(&next)
        {
            let _ = std::fs::write(dir.join(unique(".json")), json);
        }
    }
}

fn add(a: ClientReport, b: ClientReport) -> ClientReport {
    if a.calls.saturating_add(b.calls) > MAX_CALLS {
        return b;
    }
    ClientReport {
        calls: a.calls + b.calls,
        ms_total: a.ms_total.saturating_add(b.ms_total),
        ms_max: a.ms_max.max(b.ms_max),
        timeouts: a.timeouts.saturating_add(b.timeouts),
        failed_open: a.failed_open.saturating_add(b.failed_open),
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
        let waiting = load();
        let counts: ClientReport = serde_json::from_str(&waiting.header().unwrap()).unwrap();
        assert_eq!(
            counts,
            ClientReport {
                calls: 2,
                ms_total: 5040,
                ms_max: 5000,
                timeouts: 1,
                failed_open: 1
            }
        );
        waiting.settle(Duration::from_millis(30), Sent::Answered);
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

    #[test]
    fn two_calls_at_once_never_send_the_same_counts() {
        let dir = tempfile::tempdir().unwrap();
        let load = || Pending::load(Some(dir.path()), "claude", Some("s1"));
        load().settle(Duration::from_millis(5000), Sent::TimedOut);
        // Both calls start before either settles: only the first carries the waiting count.
        let first = load();
        let second = load();
        assert!(first.header().is_some());
        assert_eq!(second.header(), None);
        // Neither answered: each leaves its own file, and nothing is lost or counted twice.
        first.settle(Duration::from_millis(10), Sent::FailedOpen);
        second.settle(Duration::from_millis(20), Sent::FailedOpen);
        let all: ClientReport = serde_json::from_str(&load().header().unwrap()).unwrap();
        assert_eq!((all.calls, all.timeouts, all.failed_open), (3, 1, 2));
    }
}
