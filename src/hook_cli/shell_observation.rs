//! The working tree around a shell tool call. The pre-call snapshot is kept in a small private
//! store under the AgentSCM config directory until the call ends, when the post-call hook takes
//! it exactly once, snapshots again and reports the paths whose state changed. Whatever goes
//! wrong is reported as unknown, never as no change. Nothing here sends command text or output.
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use agentscm::git_collisions::{self, OBSERVED_PATHS, WorktreeState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The longest the pre-call snapshot may take, so the shell call is not held up for long.
pub(super) const PRE_SNAPSHOT: Duration = Duration::from_millis(700);
/// The longest the post-call snapshot and comparison may take together.
pub(super) const POST_DIFF: Duration = Duration::from_millis(1000);

/// At most this many stored snapshots, each at most [`MAX_BYTES`]: the store never holds more than
/// 64 MiB. A snapshot too large to store is reported as unknown, not as no change.
const MAX_FILES: usize = 64;
/// Time left for the Hub call when the hook's own deadline bounds an observation.
const HUB_RESERVE: Duration = Duration::from_secs(1);

/// When a snapshot or comparison must stop: its own limit, but never so late that the hook's
/// `hook_deadline` leaves less than [`HUB_RESERVE`] for the Hub call.
pub(super) fn bounded(limit: Duration, hook_deadline: Instant) -> Instant {
    let now = Instant::now();
    (now + limit).min(hook_deadline.checked_sub(HUB_RESERVE).unwrap_or(now))
}

/// A shell call the agent runs in the background: its post-call event fires at launch, so the
/// working tree then says nothing about what the call goes on to do.
pub(super) fn is_background(payload: &Value) -> bool {
    payload["tool_input"]["run_in_background"] == true
}

/// What a post-call event reports when this process could not even try to observe.
pub(super) fn local_error(payload: &Value) -> Option<Value> {
    (payload["hook_event_name"] != "PreToolUse")
        .then(|| Outcome::Unknown("local_error").to_json())
        .flatten()
}
/// A snapshot whose call never ended (its post-call hook did not run) is dropped after this.
const EXPIRY: Duration = Duration::from_secs(60 * 60);
const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    root: String,
    created: u64,
    /// `None` marks a pre-call snapshot that failed.
    state: Option<WorktreeState>,
    /// The user's global excludes file the pre-call snapshot used, so the post-call one hides the
    /// same files.
    #[serde(default)]
    excludes: Option<String>,
}

impl Stored {
    fn valid(&self, root: &Path) -> bool {
        self.root == root.to_string_lossy()
            && self.state.as_ref().is_none_or(WorktreeState::valid)
            && self
                .excludes
                .as_deref()
                .is_none_or(|path| Path::new(path).is_absolute())
    }
}

/// What the post-call hook found.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Outcome {
    Changed { paths: Vec<String>, truncated: bool },
    Unknown(&'static str),
    NoChange,
}

impl Outcome {
    /// The wire form, or `None` when nothing changed. Paths are checked again here so the payload
    /// never carries more than the bounds allow.
    pub(super) fn to_json(&self) -> Option<Value> {
        match self {
            Self::NoChange => None,
            Self::Unknown(reason) => Some(json!({"outcome": "unknown", "reason": reason})),
            Self::Changed { paths, truncated } => {
                if paths.is_empty()
                    || paths.len() > OBSERVED_PATHS
                    || !paths.iter().all(|p| git_collisions::advisory_path(p))
                {
                    return Some(json!({"outcome": "unknown", "reason": "git_failed"}));
                }
                Some(json!({"outcome": "changed", "paths": paths, "truncated": truncated}))
            }
        }
    }
}

fn store(config: &Path) -> PathBuf {
    config.join("agentscm").join("shell-observations")
}

fn key(agent: &str, session: &str, tool_use_id: &str, root: &Path) -> String {
    let text = format!(
        "shell-observation\0{agent}\0{session}\0{tool_use_id}\0{}",
        root.display()
    );
    format!("{:x}", Sha256::digest(text))
}

fn is_key(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn age(path: &Path) -> Option<Duration> {
    std::fs::symlink_metadata(path)
        .ok()?
        .modified()
        .ok()?
        .elapsed()
        .ok()
}

/// Creates the store (0700), drops what expired and makes room for one more file, oldest first.
fn prepare(dir: &Path) -> bool {
    use std::os::unix::fs::DirBuilderExt;
    if std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .is_err()
        || !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir())
    {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut kept = Vec::new();
    for entry in entries.take(MAX_FILES * 2 + 16).flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let ours = name
            .to_str()
            .is_some_and(|n| is_key(n) || n.starts_with('.'));
        if !ours || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        match age(&path) {
            Some(age) if age <= EXPIRY => kept.push((age, path)),
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    // Oldest last.
    kept.sort_by_key(|(age, _)| *age);
    while kept.len() >= MAX_FILES {
        if let Some((_, path)) = kept.pop() {
            let _ = std::fs::remove_file(path);
        }
    }
    true
}

fn write(dir: &Path, name: &str, stored: &Stored) -> bool {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let Ok(bytes) = serde_json::to_vec(stored) else {
        return false;
    };
    if bytes.len() as u64 > MAX_BYTES {
        return false;
    }
    let temp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temp)
        .and_then(|mut file| file.write_all(&bytes))
        .is_ok();
    if !written || std::fs::rename(&temp, dir.join(name)).is_err() {
        let _ = std::fs::remove_file(&temp);
        return false;
    }
    true
}

/// Reads and removes the stored file; the rename first means one hook process consumes it.
fn take(dir: &Path, name: &str) -> Option<Result<Vec<u8>, ()>> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(name);
    let taken = dir.join(format!(".{name}.take-{}", std::process::id()));
    std::fs::rename(&path, &taken).ok()?;
    let read = (|| {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
            .open(&taken)
            .map_err(|_| ())?;
        let metadata = file.metadata().map_err(|_| ())?;
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(());
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        Ok(bytes)
    })();
    let _ = std::fs::remove_file(&taken);
    Some(read)
}

/// Takes and stores the pre-call snapshot of `root`. Nothing is stored without a `tool_use_id`;
/// a failed snapshot is stored as a marker so the post-call hook says so.
pub(super) fn pre(
    config: &Path,
    agent: &str,
    session: &str,
    tool_use_id: Option<&str>,
    root: &Path,
    git: &Path,
    deadline: Instant,
) {
    let Some(id) = tool_use_id else {
        return;
    };
    let dir = store(config);
    if !prepare(&dir) {
        return;
    }
    // Resolution trouble only means ignored-by-the-user files may show; it is not an error.
    let excludes = git_collisions::global_excludes_file(git, root, deadline);
    let state = git_collisions::worktree_state(git, root, deadline, excludes.as_deref()).ok();
    let stored = Stored {
        root: root.to_string_lossy().into_owned(),
        created: now(),
        state,
        excludes: excludes.map(|path| path.to_string_lossy().into_owned()),
    };
    write(&dir, &key(agent, session, id, root), &stored);
}

/// Consumes the pre-call snapshot, snapshots `root` again and compares them.
pub(super) fn post(
    config: &Path,
    agent: &str,
    session: &str,
    tool_use_id: Option<&str>,
    root: &Path,
    git: &Path,
    deadline: Instant,
) -> Outcome {
    let Some(id) = tool_use_id else {
        return Outcome::Unknown("no_tool_use_id");
    };
    let dir = store(config);
    let Some(read) = take(&dir, &key(agent, session, id, root)) else {
        return Outcome::Unknown("missing_snapshot");
    };
    let stored = read
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Stored>(&bytes).ok())
        .filter(|stored| stored.valid(root))
        .filter(|stored| now().saturating_sub(stored.created) <= EXPIRY.as_secs());
    let Some(stored) = stored else {
        return Outcome::Unknown("invalid_snapshot");
    };
    let Some(before) = stored.state else {
        return Outcome::Unknown("pre_snapshot_failed");
    };
    let excludes = stored.excludes.as_deref().map(Path::new);
    let Ok(after) = git_collisions::worktree_state(git, root, deadline, excludes) else {
        return Outcome::Unknown("git_failed");
    };
    match git_collisions::changed_during(git, root, &before, &after, deadline) {
        Ok(observed) if observed.paths.is_empty() => Outcome::NoChange,
        Ok(observed) => Outcome::Changed {
            paths: observed.paths,
            truncated: observed.truncated,
        },
        Err(_) => Outcome::Unknown("git_failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_form_is_bounded() {
        assert_eq!(Outcome::NoChange.to_json(), None);
        let many = (0..=OBSERVED_PATHS).map(|n| format!("f{n}")).collect();
        let odd = vec!["../escape".to_owned()];
        for paths in [many, odd, Vec::new()] {
            let out = Outcome::Changed {
                paths,
                truncated: false,
            };
            assert_eq!(out.to_json().unwrap()["outcome"], "unknown");
        }
        let ok = Outcome::Changed {
            paths: vec!["src/a.rs".into()],
            truncated: true,
        };
        assert_eq!(ok.to_json().unwrap()["truncated"], true);
    }

    #[test]
    fn every_reason_sent_is_one_the_hub_accepts() {
        for reason in [
            "missing_snapshot",
            "pre_snapshot_failed",
            "invalid_snapshot",
            "git_failed",
            "no_tool_use_id",
            "background",
            "local_error",
        ] {
            assert!(
                git_collisions::OBSERVED_UNKNOWN.contains(&reason),
                "{reason}"
            );
        }
    }

    #[test]
    fn background_calls_and_local_failures_are_unknown_not_unchanged() {
        let end = json!({"hook_event_name":"PostToolUse","tool_input":{"run_in_background":true}});
        assert!(is_background(&end));
        assert!(!is_background(
            &json!({"tool_input":{"run_in_background":false}})
        ));
        assert!(!is_background(&json!({"tool_input":{"command":"ls"}})));
        assert_eq!(local_error(&end).unwrap()["reason"], "local_error");
        assert_eq!(local_error(&json!({"hook_event_name":"PreToolUse"})), None);
    }

    #[test]
    fn the_hub_call_keeps_its_reserve_of_the_hook_deadline() {
        let hook = Instant::now() + Duration::from_secs(2);
        let end = bounded(Duration::from_secs(30), hook);
        assert!(end <= hook - HUB_RESERVE);
        // Plenty of time: the observation's own limit applies.
        let far = Instant::now() + Duration::from_secs(60);
        assert!(bounded(POST_DIFF, far) < Instant::now() + POST_DIFF + Duration::from_millis(50));
        // No time at all is a deadline already passed.
        assert!(bounded(POST_DIFF, Instant::now()) <= Instant::now());
    }

    #[test]
    fn a_corrupt_snapshot_is_invalid_and_consumed() {
        let dir = tempfile::tempdir().unwrap();
        let root = Path::new("/synthetic/root");
        let store = store(dir.path());
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join(key("claude", "s", "t", root)), b"{\"bogus\":1}").unwrap();
        let args = (dir.path(), "claude", "s", Some("t"), root);
        let run = || {
            post(
                args.0,
                args.1,
                args.2,
                args.3,
                args.4,
                Path::new("/usr/bin/git"),
                Instant::now() + POST_DIFF,
            )
        };
        assert_eq!(run(), Outcome::Unknown("invalid_snapshot"));
        assert_eq!(run(), Outcome::Unknown("missing_snapshot"));
    }
}
