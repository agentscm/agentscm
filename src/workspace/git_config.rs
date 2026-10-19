//! Ambient selection settings: the user's global Git excludes file and case sensitivity.
//!
//! A workspace root with a `.git` asks Git itself (`git config`), so local, global and system
//! configuration and `[include]` are honored exactly as Git reads them. Git missing, failing or
//! slow is a typed error rather than a silent fallback. A workspace without `.git` never runs Git
//! and probes the filesystem for case sensitivity instead.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::selection::Settings;
use crate::execution::Cancellation;
use crate::execution::process::{Capture, capture_with_input};
use crate::{Error, Result};

const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_OUTPUT_BYTES: u64 = 8 * 1024;
const STDERR_EXCERPT: usize = 256;
/// Environment that selects which Git configuration is read; everything else is cleared.
const KEPT_ENVIRONMENT: [&str; 6] = [
    "HOME",
    "XDG_CONFIG_HOME",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "PATH",
];

/// The Git executable: the first `git` on PATH as an absolute path, else `/usr/bin/git`.
fn git_executable() -> PathBuf {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .filter(|directory| directory.is_absolute())
                .map(|directory| directory.join("git"))
                .find(|candidate| candidate.is_file())
        })
        .and_then(|git| git.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"))
}

pub(super) fn has_dot_git(root: &Path) -> Result<bool> {
    match std::fs::symlink_metadata(root.join(".git")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(Error::Io(error)),
    }
}

/// Settings for a scan of `root`. `case_probe` reports whether the filesystem is case
/// insensitive and is only called for a root without `.git`.
pub(super) fn ambient_settings(
    root: &Path,
    case_probe: impl FnOnce() -> Result<bool>,
) -> Result<Settings> {
    if !has_dot_git(root)? {
        return Ok(Settings {
            git_common_dir: None,
            global_excludes: None,
            case_insensitive: case_probe()?,
        });
    }
    let git = git_executable();
    let common_dir = repository_common_dir(&git, root)?;
    let excludes = git_config(&git, root, &["--path", "--get", "core.excludesFile"])?;
    let global_excludes = match excludes.map(strip_nul) {
        // An empty value means "no global file" (pinned by
        // `an_empty_core_excludes_file_means_no_global_file`).
        Some(bytes) if bytes.is_empty() => None,
        Some(bytes) => {
            let path = PathBuf::from(OsString::from_vec(bytes));
            Some(if path.is_absolute() {
                path
            } else {
                root.join(path)
            })
        }
        None => default_global_excludes(),
    };
    let ignore_case = git_config(&git, root, &["--type=bool", "--get", "core.ignorecase"])?;
    Ok(Settings {
        git_common_dir: Some(common_dir),
        global_excludes,
        case_insensitive: ignore_case.is_some_and(|value| strip_nul(value) == b"true"),
    })
}

/// Ask Git for the repository's work tree and common directory. Git must accept the repository
/// (ownership checks included) and its work tree must be exactly this root; otherwise `config`
/// could report "unset" for a repository Git refused to read, or read another repository's
/// settings. Returns the common directory, which holds `info/exclude` (for a linked worktree or
/// a separate git dir too).
fn repository_common_dir(git: &Path, root: &Path) -> Result<PathBuf> {
    let output = git_command(
        git,
        root,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--git-common-dir",
        ],
    )?;
    if output.code != Some(0) {
        return Err(Error::Invalid(format!(
            "Git refused the repository at the workspace root: {}",
            excerpt(&output)
        )));
    }
    // One path per line, each ended by exactly one newline. A path containing a newline would
    // make the lines ambiguous, so it is refused.
    let lines = output
        .stdout
        .strip_suffix(b"\n")
        .map(|body| body.split(|&byte| byte == b'\n').collect::<Vec<_>>());
    let Some([toplevel, common]) = lines.as_deref() else {
        return Err(Error::Invalid(
            "Git reported repository paths that cannot be read unambiguously".to_owned(),
        ));
    };
    let toplevel = PathBuf::from(OsString::from_vec(toplevel.to_vec()));
    let same = match (toplevel.canonicalize(), root.canonicalize()) {
        (Ok(toplevel), Ok(root)) => toplevel == root,
        _ => false,
    };
    if !same {
        return Err(Error::Invalid(
            "the workspace root's .git does not own the workspace root".to_owned(),
        ));
    }
    Ok(PathBuf::from(OsString::from_vec(common.to_vec())))
}

/// Git's default when `core.excludesFile` is unset.
fn default_global_excludes() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|value| !value.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })?;
    Some(config.join("git").join("ignore"))
}

/// Remove exactly the NUL that `git config -z` ends a value with; a path may end in a newline.
fn strip_nul(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() == Some(&0) {
        bytes.pop();
    }
    bytes
}

fn excerpt(output: &Capture) -> String {
    let end = output.stderr.len().min(STDERR_EXCERPT);
    String::from_utf8_lossy(&output.stderr[..end])
        .trim()
        .to_owned()
}

/// `git config -z <args>` in `root`; `None` when the key is unset (exit 1). Only meaningful
/// after [`require_repository_root`] has shown Git accepts the repository.
fn git_config(git: &Path, root: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let mut full = vec!["config", "-z"];
    full.extend_from_slice(args);
    let output = git_command(git, root, &full)?;
    match output.code {
        Some(0) => Ok(Some(output.stdout)),
        Some(1) => Ok(None),
        code => Err(Error::Invalid(format!(
            "git config failed with {code:?}: {}",
            excerpt(&output)
        ))),
    }
}

fn git_command(git: &Path, root: &Path, args: &[&str]) -> Result<Capture> {
    let mut command = Command::new(git);
    command.env_clear();
    for name in KEPT_ENVIRONMENT {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        // Own process group so a timeout also stops anything Git started.
        .process_group(0)
        .arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args);
    let output = capture_with_input(
        &mut command,
        None,
        GIT_TIMEOUT,
        GIT_OUTPUT_BYTES,
        &Cancellation::default(),
    )
    .map_err(|error| {
        Error::Invalid(format!(
            "git is required to read the workspace's exclusion settings: {error}"
        ))
    })?;
    if let Some(stop) = output.stop {
        return Err(Error::Invalid(format!(
            "git {} did not complete ({stop:?})",
            args.first().copied().unwrap_or("")
        )));
    }
    Ok(output)
}
