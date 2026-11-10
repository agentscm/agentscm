//! The hook client's own measure of its Hub calls: how long each took, and whether it timed out
//! or failed open. A call that timed out never reached the Hub, so the counts wait in small files
//! per agent session and travel with the next call (`x-agentscm-hook-report`). No extra request
//! is ever made for them.
//!
//! Each measurement is its own file, and a call takes waiting files by renaming them first, so two
//! hooks of one session running at once never send the same counts. A report carries an id. When
//! the call carrying it is not answered, the Hub may still have received it (a relayed request
//! outlives the hook), so the report is kept as sent and sent again unchanged, with the same id,
//! until a call is answered; the Hub counts each id once.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agentscm::hub::hook_overhead::ClientReport;
use sha2::{Digest, Sha256};

/// The Hub ignores a report claiming more calls than this; past it the counts start again.
const MAX_CALLS: u64 = 10_000;
const MAX_CALL_MS: u64 = 600_000;
/// Waiting files one call takes; any more wait for the next call.
const MAX_FILES: usize = 100;
/// A session directory untouched this long is removed when another session starts one.
const STALE: Duration = Duration::from_secs(7 * 86_400);
const SENT: &str = ".sent.json";

/// How one measured call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// The Hub answered, so the report this call carried arrived with it.
    Answered,
    TimedOut,
    FailedOpen,
}

/// The report this call took for its agent session, and where that session's files are kept.
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
    format!("{nanos:020}-{}-{n}{suffix}", std::process::id())
}

/// A report id: not secret, only distinct per report.
fn new_id() -> u64 {
    let digest = Sha256::digest(unique(""));
    u64::from_be_bytes(digest[..8].try_into().expect("eight bytes"))
}

/// Takes a waiting file by renaming it first; None when another call took it.
fn take(path: &Path) -> Option<ClientReport> {
    let taken = path.with_extension(unique(".taken"));
    std::fs::rename(path, &taken).ok()?;
    let report = std::fs::read(&taken)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let _ = std::fs::remove_file(&taken);
    report
}

impl Pending {
    /// Takes the counts to send for this agent session: a report already sent without an answer,
    /// unchanged with its id; otherwise every waiting count, as one report with a new id. Empty
    /// when there are none, no session or no state directory.
    pub fn load(config: Option<&Path>, agent: &str, session: Option<&str>) -> Self {
        let dir = config.zip(session).map(|(config, session)| {
            let key = Sha256::digest(format!("{agent}\0{session}"));
            config.join("hook-report").join(format!("{key:x}"))
        });
        let mut waiting: Vec<PathBuf> = dir
            .as_deref()
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        waiting.sort();
        let mut report = ClientReport::default();
        let resend = waiting
            .iter()
            .filter(|path| path.to_string_lossy().ends_with(SENT))
            .find_map(|path| take(path));
        if let Some(sent) = resend {
            report = sent;
        } else {
            for path in waiting.iter().take(MAX_FILES) {
                if let Some(counts) = take(path) {
                    report = add(report, counts);
                }
            }
            if report.calls > 0 {
                report.id = Some(new_id());
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

    /// Leaves this call's counts in a new file for a later call. When the call was not answered,
    /// the report it carried is kept as sent, with its id, to go out again unchanged. Best effort:
    /// a lost write loses counts and never affects the hook.
    pub fn settle(self, took: Duration, sent: Sent) {
        let Some(dir) = self.dir else { return };
        let ms = u64::try_from(took.as_millis())
            .unwrap_or(MAX_CALL_MS)
            .min(MAX_CALL_MS);
        let this = ClientReport {
            id: None,
            calls: 1,
            ms_total: ms,
            ms_max: ms,
            timeouts: u64::from(sent == Sent::TimedOut),
            failed_open: u64::from(sent == Sent::FailedOpen),
        };
        let new = !dir.exists();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        if new && let Some(root) = dir.parent() {
            prune_stale(root);
        }
        let write = |report: &ClientReport, suffix: &str| {
            if let Ok(json) = serde_json::to_vec(report) {
                let _ = std::fs::write(dir.join(unique(suffix)), json);
            }
        };
        if sent != Sent::Answered && self.report.calls > 0 {
            write(&self.report, SENT);
        }
        write(&this, ".json");
    }
}

/// Removes session directories nothing has written to for [`STALE`].
fn prune_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|at| at.elapsed().ok())
            .is_some_and(|age| age > STALE);
        if old && entry.path().is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

fn add(a: ClientReport, b: ClientReport) -> ClientReport {
    if a.calls.saturating_add(b.calls) > MAX_CALLS {
        return b;
    }
    ClientReport {
        id: None,
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

    fn counts(pending: &Pending) -> ClientReport {
        serde_json::from_str(&pending.header().unwrap()).unwrap()
    }

    #[test]
    fn unanswered_calls_wait_for_the_next_answered_one_and_then_start_again() {
        let dir = tempfile::tempdir().unwrap();
        let load = || Pending::load(Some(dir.path()), "claude", Some("s1"));
        assert_eq!(load().header(), None);
        load().settle(Duration::from_millis(5000), Sent::TimedOut);
        load().settle(Duration::from_millis(40), Sent::FailedOpen);
        // The second call carried the first one's counts and was not answered: they are kept
        // as sent, and the next call sends them again unchanged.
        let resent = load();
        let first = counts(&resent);
        assert_eq!((first.calls, first.timeouts), (1, 1));
        assert!(first.id.is_some());
        resent.settle(Duration::from_millis(30), Sent::Answered);
        // Answered: the next call sends the waiting counts as a new report with a new id.
        let next = counts(&load());
        assert_eq!((next.calls, next.ms_total, next.failed_open), (2, 70, 1));
        assert_ne!(next.id, first.id);
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
    fn a_report_sent_on_a_call_that_timed_out_goes_again_with_the_same_id() {
        let dir = tempfile::tempdir().unwrap();
        let load = || Pending::load(Some(dir.path()), "claude", Some("s1"));
        load().settle(Duration::from_millis(5000), Sent::TimedOut);
        let carried = load();
        let sent = counts(&carried);
        // The Hub may have received it although the hook gave up.
        carried.settle(Duration::from_millis(3000), Sent::TimedOut);
        assert_eq!(counts(&load()), sent);
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
        first.settle(Duration::from_millis(10), Sent::Answered);
        second.settle(Duration::from_millis(20), Sent::Answered);
        let all = counts(&load());
        assert_eq!((all.calls, all.timeouts), (2, 0));
    }

    #[test]
    fn two_unanswered_calls_at_once_keep_both_reports_and_send_each_once() {
        let dir = tempfile::tempdir().unwrap();
        let load = || Pending::load(Some(dir.path()), "claude", Some("s1"));
        let (a, b) = (load(), load());
        a.settle(Duration::from_millis(5000), Sent::TimedOut);
        b.settle(Duration::from_millis(5000), Sent::TimedOut);
        // The first call takes both waiting counts; the second starts with nothing.
        let (first, second) = (load(), load());
        let carried = counts(&first);
        assert_eq!(carried.calls, 2);
        assert_eq!(second.header(), None);
        first.settle(Duration::from_millis(10), Sent::TimedOut);
        second.settle(Duration::from_millis(20), Sent::TimedOut);
        // The unanswered report goes again unchanged, once; then the two new counts.
        let resent = load();
        assert_eq!(counts(&resent), carried);
        resent.settle(Duration::from_millis(30), Sent::Answered);
        let next = counts(&load());
        assert_eq!((next.calls, next.timeouts), (3, 2));
        assert_ne!(next.id, carried.id);
    }

    #[test]
    fn stale_session_directories_are_removed_when_a_new_one_starts() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("hook-report");
        let old = root.join("old-session");
        std::fs::create_dir_all(&old).unwrap();
        let long_ago = SystemTime::now() - Duration::from_secs(8 * 86_400);
        std::fs::File::open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        Pending::load(Some(dir.path()), "claude", Some("new"))
            .settle(Duration::ZERO, Sent::Answered);
        assert!(!old.exists());
    }
}
