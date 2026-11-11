//! Whether this computer's person is at it, for a hook that would otherwise hide a prompt from
//! them.
//!
//! Some hosts show their own permission prompt only after the hook that offers it to the Hub
//! returns. Such a hook may hold the prompt only while nobody is at the computer, and gives it
//! back the moment someone is. Only the whole machine's keyboard and pointer can say nobody is
//! there, and that is read on macOS alone. Input in the terminal the hook runs under (on any
//! Unix) says someone is, but its quiet does not say they are gone: they may be in another
//! window. Where the machine's idle time cannot be read the answer is not known, which is never
//! taken as away.
use std::time::Duration;

/// Without input for this long, the person is taken to be away.
const AWAY_AFTER: Duration = Duration::from_secs(60);
/// How often a hook that holds a prompt looks again.
const LOOK_EVERY: Duration = Duration::from_secs(2);
/// A program asked for the idle time must answer within this.
const ASK_TIMEOUT: Duration = Duration::from_secs(2);
/// Debug builds only: the idle time in milliseconds in place of the system's, or the path of a
/// file holding it, read at each look.
const IDLE_OVERRIDE: &str = "AGENTSCM_TEST_IDLE_MS";

/// Whether nobody has been at this computer for [`AWAY_AFTER`]. False when that is not known.
pub(super) async fn away() -> bool {
    let asked = tokio::task::spawn_blocking(nobody_there);
    matches!(tokio::time::timeout(ASK_TIMEOUT, asked).await, Ok(Ok(true)))
}

/// Resolves when someone is at the computer again, or when that can no longer be told.
pub(super) async fn returned() {
    loop {
        tokio::time::sleep(LOOK_EVERY).await;
        if !away().await {
            return;
        }
    }
}

/// The machine has gone without input for [`AWAY_AFTER`], and so has this hook's terminal where
/// it has one.
fn nobody_there() -> bool {
    let quiet = |idle: Option<Duration>| idle.is_some_and(|idle| idle >= AWAY_AFTER);
    // The override never takes effect in a release build, so it cannot hide a prompt there.
    if cfg!(debug_assertions)
        && let Ok(value) = std::env::var(IDLE_OVERRIDE)
    {
        return quiet(test_idle(&value));
    }
    quiet(machine_idle()) && terminal_idle().is_none_or(|idle| idle >= AWAY_AFTER)
}

fn test_idle(value: &str) -> Option<Duration> {
    let ms = value.parse().ok().or_else(|| {
        std::fs::read_to_string(value)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()
    })?;
    Some(Duration::from_millis(ms))
}

#[cfg(target_os = "macos")]
fn machine_idle() -> Option<Duration> {
    let listed = std::process::Command::new("/usr/sbin/ioreg")
        .args(["-c", "IOHIDSystem", "-d", "4"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    hid_idle(&String::from_utf8_lossy(&listed.stdout))
}

#[cfg(not(target_os = "macos"))]
fn machine_idle() -> Option<Duration> {
    None
}

/// `"HIDIdleTime" = <nanoseconds>` in `ioreg`'s listing of the input system.
#[cfg(any(target_os = "macos", test))]
fn hid_idle(listing: &str) -> Option<Duration> {
    let rest = listing.split("\"HIDIdleTime\" = ").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok().map(Duration::from_nanos)
}

/// Since a key was last read from the terminal this hook runs under, as `w` counts idle time.
#[cfg(unix)]
fn terminal_idle() -> Option<Duration> {
    let named = std::process::Command::new("ps")
        .args(["-o", "tty=", "-p", &std::process::id().to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let device = terminal_device(String::from_utf8_lossy(&named.stdout).trim())?;
    std::fs::metadata(device)
        .ok()?
        .accessed()
        .ok()?
        .elapsed()
        .ok()
}

#[cfg(not(unix))]
fn terminal_idle() -> Option<Duration> {
    None
}

/// The device of a terminal as `ps` names it (`ttys004`, `pts/3`); none for no terminal (`??`).
#[cfg(any(unix, test))]
fn terminal_device(name: &str) -> Option<std::path::PathBuf> {
    let named = !name.is_empty()
        && name
            .split('/')
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric()));
    named.then(|| std::path::Path::new("/dev").join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_time_is_read_from_the_systems_own_words_and_unknown_is_not_away() {
        let listing = "    | |   \"HIDKeyboardModifierMappingPairs\" = ()\n    | |   \"HIDIdleTime\" = 79085991500\n    | |   \"HIDPointerAcceleration\" = 45056\n";
        assert_eq!(
            hid_idle(listing),
            Some(Duration::from_nanos(79_085_991_500))
        );
        assert_eq!(hid_idle("\"HIDIdleTime\" = soon"), None);
        assert_eq!(hid_idle("no input system"), None);
        assert_eq!(test_idle("1500"), Some(Duration::from_millis(1500)));
        assert_eq!(test_idle("/nonexistent/idle"), None);

        let device = |name: &str| terminal_device(name);
        assert_eq!(device("ttys004"), Some("/dev/ttys004".into()));
        assert_eq!(device("pts/3"), Some("/dev/pts/3".into()));
        for none in ["", "??", "?", "-", "../etc/passwd", "pts//3", "/pts/3"] {
            assert_eq!(device(none), None, "{none}");
        }
    }
}
