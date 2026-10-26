//! Unix producer and Git observation. Git is only ever asked read-only questions;
//! recipe commands run as the signing user with a cleared, allowlisted environment (HOME is passed).

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::{
    CommandResult, GitValidationContext, GitValidationSubject, Services, StopReason,
    ValidationPolicy, ValidationRecipe, ValidationRecord,
};
use crate::execution::Cancellation;
use crate::execution::process::{Stop, capture_with_input};
use crate::{Error, Result};

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);
const GIT_OUTPUT: u64 = 16 * 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PROBE_OUTPUT: u64 = 64 * 1024;
/// Retained bytes per recipe command, stdout and stderr together.
pub const COMMAND_OUTPUT_LIMIT: u64 = 16 * 1024 * 1024;
const ENV_ALLOWLIST: [&str; 8] = [
    "PATH",
    "HOME",
    "USER",
    "TMPDIR",
    "LANG",
    "CARGO_HOME",
    "RUSTUP_HOME",
    "CARGO_TARGET_DIR",
];
const S3_VARS: [&str; 4] = [
    "AGENTSCM_TEST_S3_ENDPOINT",
    "AGENTSCM_TEST_S3_BUCKET",
    "AGENTSCM_TEST_S3_ACCESS_KEY_ID",
    "AGENTSCM_TEST_S3_SECRET_ACCESS_KEY",
];

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn allowlisted(command: &mut Command) {
    command.env_clear();
    for (key, value) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if ENV_ALLOWLIST.contains(&&*name) || name.starts_with("AGENTSCM_TEST_") {
            command.env(key, value);
        }
    }
}

fn git(repo: &Path, args: &[&str], limit: u64) -> Result<Vec<u8>> {
    git_timeout(repo, args, limit, GIT_TIMEOUT, false)
}

fn git_timeout(
    repo: &Path,
    args: &[&str],
    limit: u64,
    timeout: Duration,
    network: bool,
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("LC_ALL", "C")
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .arg("-C")
        .arg(repo)
        .args(["--no-optional-locks", "-c", "core.fsmonitor=false"]);
    if network {
        // No credentials, prompts or environment configuration reach the transport.
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .args(["-c", "credential.helper="]);
    }
    command.args(args).process_group(0);
    let out = capture_with_input(&mut command, None, timeout, limit, &Cancellation::default())?;
    if out.stop.is_some() || out.code != Some(0) {
        return Err(Error::Invalid(format!(
            "git {} failed",
            args.first().copied().unwrap_or("")
        )));
    }
    Ok(out.stdout)
}

fn git_line(repo: &Path, args: &[&str]) -> Result<String> {
    let out = git(repo, args, 4096)?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

fn rev(repo: &Path, name: &str, suffix: &str) -> Result<String> {
    if name.starts_with('-') || name.contains('\0') {
        return Err(Error::Invalid("revision must not start with '-'".into()));
    }
    git_line(repo, &["rev-parse", "--verify", &format!("{name}{suffix}")])
}

/// What Git says about a head and its base, read-only.
pub struct GitState {
    pub commit: String,
    pub tree: String,
    pub base_tip: String,
    pub merge_base: String,
}

pub fn observe(repo: &Path, head: &str, base_rev: &str) -> Result<GitState> {
    let commit = rev(repo, head, "^{commit}")?;
    let base_tip = rev(repo, base_rev, "^{commit}")?;
    Ok(GitState {
        tree: rev(repo, &commit, "^{tree}")?,
        merge_base: git_line(repo, &["merge-base", &commit, &base_tip])?,
        commit,
        base_tip,
    })
}

/// Number of paths that differ between two trees and match `pathspecs` (all paths when empty).
/// Plumbing, so renames, external diff and `diff.relative` configuration cannot change the count.
fn changed(repo: &Path, from: &str, to: &str, pathspecs: &[String]) -> Result<usize> {
    for name in [from, to] {
        if name.starts_with('-') || name.contains('\0') {
            return Err(Error::Invalid("revision must not start with '-'".into()));
        }
    }
    let mut args = vec!["diff-tree", "-r", "--name-only", "-z", "--no-renames"];
    args.extend([from, to, "--"]);
    args.extend(pathspecs.iter().map(String::as_str));
    Ok(git(repo, &args, GIT_OUTPUT)?
        .iter()
        .filter(|b| **b == 0)
        .count())
}

/// Built-in pathspecs for the files that define the gate itself, relative to `repo`.
pub fn trust_paths(config_dir: &str, verifier_workflow: &str) -> Vec<String> {
    [config_dir, verifier_workflow]
        .map(|path| format!(":(literal){path}"))
        .into()
}

/// Recipe command ids the change requires, in recipe order. A change to any trust input
/// requires every command.
pub fn required_commands(
    repo: &Path,
    merge_base: &str,
    head: &str,
    recipe: &ValidationRecipe,
    trust: &[String],
) -> Result<Vec<String>> {
    let all = changed(repo, merge_base, head, trust)? > 0;
    let mut ids = Vec::new();
    for command in &recipe.commands {
        let required = match &command.paths {
            Some(paths) if !all => changed(repo, merge_base, head, paths)? > 0,
            _ => true,
        };
        if required {
            ids.push(command.id.clone());
        }
    }
    Ok(ids)
}

/// True when the change is non-empty, touches no trust input and every path matches `exempt`.
pub fn only_exempt(
    repo: &Path,
    merge_base: &str,
    head: &str,
    exempt: &[String],
    trust: &[String],
) -> Result<bool> {
    let total = changed(repo, merge_base, head, &[])?;
    Ok(total > 0
        && changed(repo, merge_base, head, trust)? == 0
        && changed(repo, merge_base, head, exempt)? == total)
}

fn fetch_base(repo: &Path, base: &str) -> Result<()> {
    let base_spec = format!("+refs/heads/{base}:refs/remotes/origin/{base}");
    git_timeout(
        repo,
        &["fetch", "--no-tags", "--quiet", "origin", &base_spec],
        GIT_OUTPUT,
        FETCH_TIMEOUT,
        true,
    )
    .map(drop)
}

/// Fetches the base branch and `refs/pull/{number}/head` from `origin` (Git data only, nothing is
/// checked out) and requires the pull ref to name exactly `head`.
pub fn fetch_pull(repo: &Path, base: &str, number: i64, head: &str) -> Result<()> {
    fetch_base(repo, base)?;
    let pull_ref = format!("refs/pull/{number}/head");
    git_timeout(
        repo,
        &["fetch", "--no-tags", "--quiet", "origin", &pull_ref],
        GIT_OUTPUT,
        FETCH_TIMEOUT,
        true,
    )?;
    if rev(repo, "FETCH_HEAD", "^{commit}")? != head {
        return Err(Error::Invalid(
            "pull request head moved while fetching".into(),
        ));
    }
    Ok(())
}

/// Tree id of `dir` at the checkout's `HEAD`, resolved from `repo` (not the repository root).
pub fn config_dir_tree(repo: &Path, dir: &str) -> Result<String> {
    rev(repo, "HEAD", &format!(":./{dir}"))
}

/// True when `dir` has uncommitted changes, untracked and ignored files included.
pub fn config_dir_dirty(repo: &Path, dir: &str) -> Result<bool> {
    Ok(!git(
        repo,
        &[
            "status",
            "--porcelain",
            "--untracked-files=normal",
            "--ignored=matching",
            "--",
            dir,
        ],
        GIT_OUTPUT,
    )?
    .is_empty())
}

/// Re-fetches `origin/{base}` and tells whether `dir` (relative to `repo`) has the same tree
/// there as at the checkout's `HEAD`. A directory missing at `origin/{base}` has changed. Err when
/// the fetch fails or either side cannot be read otherwise.
pub fn config_dir_unchanged(repo: &Path, base: &str, dir: &str) -> Result<bool> {
    fetch_base(repo, base)?;
    let tip = format!("refs/remotes/origin/{base}");
    rev(repo, &tip, "^{commit}")?;
    let Ok(theirs) = rev(repo, &tip, &format!(":./{dir}")) else {
        return Ok(false);
    };
    Ok(config_dir_tree(repo, dir)? == theirs)
}

fn is_clean(repo: &Path) -> Result<bool> {
    Ok(git(
        repo,
        &["status", "--porcelain", "--untracked-files=normal"],
        GIT_OUTPUT,
    )?
    .is_empty())
}

/// Digest of `path` at `commit`; a file the commit does not contain is an error.
fn file_digest(repo: &Path, commit: &str, path: &str) -> Result<String> {
    git(
        repo,
        &["cat-file", "blob", &format!("{commit}:{path}")],
        GIT_OUTPUT,
    )
    .map(|bytes| sha256(&bytes))
    .map_err(|_| Error::Invalid(format!("{path} cannot be read at the commit")))
}

/// Captured bytes of one command, exactly those the record's digests cover.
pub struct CommandOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub struct Produced {
    pub record: ValidationRecord,
    /// One entry per `record.results`, in order.
    pub outputs: Vec<CommandOutput>,
    /// (path, sha256) of each recipe dependency file at the commit, in recipe order.
    pub dependencies: Vec<(String, String)>,
}

fn probe(repo: &Path, argv: &[String]) -> String {
    let mut command = Command::new(&argv[0]);
    allowlisted(&mut command);
    command.args(&argv[1..]).current_dir(repo).process_group(0);
    let line = capture_with_input(
        &mut command,
        None,
        PROBE_TIMEOUT,
        PROBE_OUTPUT,
        &Cancellation::default(),
    )
    .ok()
    .filter(|c| c.stop.is_none() && c.code == Some(0))
    .and_then(|c| {
        String::from_utf8_lossy(&c.stdout)
            .lines()
            .next()
            .map(|l| l.chars().take(512).collect::<String>())
    })
    .filter(|l| !l.is_empty());
    line.unwrap_or_else(|| {
        let unavailable = format!("{}: unavailable", argv.join(" "));
        unavailable.chars().take(512).collect()
    })
}

fn run_command(
    repo: &Path,
    id: &str,
    argv: &[String],
    timeout_secs: u64,
    cancel: &Cancellation,
) -> (CommandResult, CommandOutput) {
    let mut command = Command::new(&argv[0]);
    allowlisted(&mut command);
    command.args(&argv[1..]).current_dir(repo).process_group(0);
    let started = Instant::now();
    let captured = capture_with_input(
        &mut command,
        None,
        Duration::from_secs(timeout_secs),
        COMMAND_OUTPUT_LIMIT,
        cancel,
    );
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let spawn_failed = captured.is_err();
    let (code, stdout, stderr, stop) = match captured {
        Ok(c) => (c.code, c.stdout, c.stderr, c.stop),
        Err(_) => (None, vec![], vec![], None),
    };
    let reason = match stop {
        Some(Stop::Timeout) => StopReason::TimedOut,
        Some(Stop::Output) => StopReason::OutputLimit,
        Some(Stop::Cancelled) => StopReason::Cancelled,
        Some(Stop::Io) => StopReason::IoError,
        None if spawn_failed => StopReason::SpawnFailed,
        // No exit code without a stop: the child died to a signal.
        None if code.is_none() => StopReason::IoError,
        None => StopReason::Exited,
    };
    let result = CommandResult {
        id: id.into(),
        argv: argv.to_vec(),
        exit_code: if reason == StopReason::Exited {
            code
        } else {
            None
        },
        stop: reason,
        duration_ms,
        stdout_sha256: sha256(&stdout),
        stderr_sha256: sha256(&stderr),
        truncated: reason == StopReason::OutputLimit,
    };
    (result, CommandOutput { stdout, stderr })
}

/// Runs the recipe in `repo` (which must be clean) until the first failure.
/// Only commands that actually ran are recorded. `cancel` stops the running command and the run.
pub fn produce(
    repo: &Path,
    recipe: &ValidationRecipe,
    policy: &ValidationPolicy,
    base_ref: &str,
    base_rev: &str,
    cancel: &Cancellation,
) -> Result<Produced> {
    let recipe_sha256 = recipe.sha256()?;
    if recipe_sha256 != policy.recipe_sha256 {
        return Err(Error::Invalid("recipe differs from the policy".into()));
    }
    if !is_clean(repo)? {
        return Err(Error::Invalid("working tree is not clean".into()));
    }
    let state = observe(repo, "HEAD", base_rev)?;
    let dependencies = recipe
        .dependency_file_list()
        .into_iter()
        .map(|path| file_digest(repo, &state.commit, &path).map(|digest| (path, digest)))
        .collect::<Result<Vec<_>>>()?;
    let (mut results, mut outputs) = (Vec::new(), Vec::new());
    for c in &recipe.commands {
        let (result, output) = run_command(repo, &c.id, &c.argv, c.timeout_secs, cancel);
        let ok = result.stop == StopReason::Exited && result.exit_code == Some(0);
        results.push(result);
        outputs.push(output);
        if !ok {
            break;
        }
    }
    let toolchain = recipe
        .toolchain_probe_list()
        .iter()
        .map(|argv| probe(repo, argv))
        .collect();
    // A failed re-check must not drop the run: it is recorded as a changed tree.
    let after = observe(repo, "HEAD", base_rev).ok();
    let unchanged = after
        .as_ref()
        .is_some_and(|a| a.commit == state.commit && a.tree == state.tree)
        && is_clean(repo).unwrap_or(false);
    let var_set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
    Ok(Produced {
        outputs,
        dependencies,
        record: ValidationRecord {
            subject: GitValidationSubject {
                repository: policy.repository.clone(),
                commit: state.commit.clone(),
                tree: state.tree.clone(),
            },
            context: GitValidationContext {
                base_ref: base_ref.into(),
                merge_base: state.merge_base,
            },
            base_tip: state.base_tip,
            recipe_sha256,
            results,
            services: Services {
                postgres: var_set("AGENTSCM_TEST_DATABASE_URL"),
                s3: S3_VARS.iter().all(|v| var_set(v)),
            },
            platform: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            toolchain,
            tree_unchanged_after: unchanged,
        },
    })
}
