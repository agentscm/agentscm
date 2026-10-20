//! `agentscm autopsy`: a local, read-only report of how often your own Claude Code sessions
//! touched a repository file another of them still held, by the hub's contested-file rule.
//! It reads Claude Code logs and runs read-only Git commands; nothing is uploaded.
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use agentscm::autopsy::{self, NearMiss};
use agentscm::hub::advice::sanitize;
use agentscm::hub::session::{HOLD_CAP_SECONDS, SESSION_TTL_SECONDS};
use agentscm::{Error, Result};
use clap::Args;

use crate::intent_cli::repo_root;

const DAY: i64 = 86_400;
const LISTED: usize = 5;

#[derive(Debug, Args)]
pub struct AutopsyArgs {
    /// Days of history to read, 1 to 90.
    #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u32).range(1..=90))]
    days: u32,
    /// Print `<file>` instead of repository file paths.
    #[arg(long)]
    no_paths: bool,
    /// A directory inside the repository. Defaults to the current directory.
    #[arg(long, value_name = "DIR")]
    repo: Option<PathBuf>,
    /// Claude Code's logs. Defaults to ~/.claude/projects.
    #[arg(long, value_name = "DIR")]
    logs: Option<PathBuf>,
    /// Absolute path of the Git executable. Defaults to the first `git` on PATH.
    #[arg(long)]
    git: Option<PathBuf>,
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn tilde(path: &Path) -> String {
    match home().and_then(|home| path.strip_prefix(home).ok().map(Path::to_path_buf)) {
        Some(rest) => Path::new("~").join(rest).display().to_string(),
        None => path.display().to_string(),
    }
}

/// Standard output of a successful, read-only Git command; no shell and a minimal environment.
fn git_text(git: &Path, root: &Path, args: &[&str]) -> Option<String> {
    let mut command = Command::new(git);
    command
        .arg("-C")
        .arg(root)
        .args(args)
        .env_clear()
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let out = command.output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

struct Logs {
    edits: Vec<autopsy::Edit>,
    files: usize,
    unreadable: usize,
}

/// `<project>/<session>.jsonl`, and the subagent logs of a session, `<project>/<session>/subagents/*.jsonl`,
/// which carry the parent session's id.
fn log_files(dir: &Path) -> (Vec<PathBuf>, usize) {
    let (mut found, mut unreadable) = (Vec::new(), 0);
    let mut jsonl = |dir: &Path, found: &mut Vec<PathBuf>| match std::fs::read_dir(dir) {
        Ok(entries) => found.extend(
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl")),
        ),
        Err(_) => unreadable += 1,
    };
    for project in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let project = project.path();
        if !project.is_dir() {
            continue;
        }
        jsonl(&project, &mut found);
        for session in std::fs::read_dir(&project).into_iter().flatten().flatten() {
            let subagents = session.path().join("subagents");
            if subagents.is_dir() {
                jsonl(&subagents, &mut found);
            }
        }
    }
    (found, unreadable)
}

fn read_logs(dir: &Path, roots: &[String], since: i64) -> Result<Logs> {
    std::fs::read_dir(dir).map_err(|error| {
        Error::Invalid(format!("cannot read logs in {}: {error}", dir.display()))
    })?;
    let (paths, unreadable) = log_files(dir);
    let mut logs = Logs {
        edits: Vec::new(),
        files: 0,
        unreadable,
    };
    for path in paths {
        // A file last written before the window cannot hold an edit inside it.
        let modified = path
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
        if modified.is_some_and(|age| i64::try_from(age.as_secs()).unwrap_or(0) < since - DAY) {
            continue;
        }
        match File::open(&path) {
            Ok(file) => {
                logs.files += 1;
                logs.edits
                    .extend(autopsy::read_edits(BufReader::new(file), roots, since));
            }
            Err(_) => logs.unreadable += 1,
        }
    }
    Ok(logs)
}

/// The repository root and every linked worktree, as written in the logs and canonicalised,
/// as text. A Windows `\\?\` prefix is dropped: logs hold plain `C:\...` paths.
fn repo_roots(start: &Path, root: &Path, git: Option<&Path>) -> Vec<String> {
    let mut roots = vec![root.to_path_buf()];
    // Logs hold the path as Claude Code saw it, which may be a symlink spelling.
    if let Ok(raw) = std::path::absolute(start)
        && let Some(dir) = raw.ancestors().find(|dir| dir.join(".git").exists())
    {
        roots.push(dir.to_path_buf());
    }
    if let Some(text) =
        git.and_then(|git| git_text(git, root, &["worktree", "list", "--porcelain"]))
    {
        for path in text.lines().filter_map(|l| l.strip_prefix("worktree ")) {
            roots.extend(Path::new(path).canonicalize().ok());
            roots.push(PathBuf::from(path));
        }
    }
    let mut roots: Vec<String> = roots
        .iter()
        .map(|root| {
            let text = root.to_string_lossy();
            text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned()
        })
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// (near-misses on a file that conflicts in `git merge-tree` today, near-misses that could be
/// checked). Checkable: both sessions' branches still exist locally, differ, and were compared
/// by the collision scan; a conflict whose file list was cut short and does not name the file
/// is left out.
#[cfg(unix)]
fn merge_conflicts(
    git: Option<&Path>,
    root: &Path,
    near: &[NearMiss],
) -> std::result::Result<(usize, usize), String> {
    use std::collections::BTreeSet;
    use std::time::{Duration, Instant};

    use agentscm::git_collisions::{self, Request};

    let git = git.ok_or("git was not found")?;
    let named = near
        .iter()
        .flat_map(|miss| [&miss.branch, &miss.holder_branch])
        .flatten()
        .filter(|name| !name.is_empty() && *name != "HEAD")
        .collect::<BTreeSet<_>>();
    let exists = |name: &str| {
        let verify = [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{name}"),
        ];
        git_text(git, root, &verify).is_some()
    };
    let branches: BTreeSet<&str> = named
        .into_iter()
        .map(String::as_str)
        .filter(|name| exists(name))
        .collect();
    let pairs: Vec<(&NearMiss, &str, &str)> = near
        .iter()
        .filter_map(|miss| {
            let (a, b) = (miss.branch.as_deref()?, miss.holder_branch.as_deref()?);
            (a != b && branches.contains(a) && branches.contains(b)).then_some((miss, a, b))
        })
        .collect();
    if pairs.is_empty() {
        return Ok((0, 0));
    }
    let report = git_collisions::collisions(&Request {
        git: git.to_path_buf(),
        repository: root.to_path_buf(),
        base: None,
        refs: branches
            .iter()
            .map(|name| format!("refs/heads/{name}"))
            .collect(),
        worktrees: false,
        active_days: None,
        include_merged: true,
        deadline: Some(Instant::now() + Duration::from_secs(30)),
        max_bytes: None,
    })
    .map_err(|error| error.to_string())?;
    if let Some(reason) = report["truncated"].as_str() {
        return Err(format!("stopped early ({reason})"));
    }
    let strings = |list: &serde_json::Value| -> Vec<String> {
        list.as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    };
    // The scan names a branch by its first ref, skips ones with no commits beyond the base, and
    // reports only pairs that share a path.
    let compared: Vec<&str> = report["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item["id"].as_str())
        .collect();
    let (mut checked, mut hit) = (0, 0);
    for (miss, a, b) in pairs {
        if !compared.contains(&a) || !compared.contains(&b) {
            continue;
        }
        let entry = report["collisions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| {
                let items = strings(&c["items"]);
                items.iter().any(|i| i == a) && items.iter().any(|i| i == b)
            });
        let Some(merge) = entry
            .map(|c| &c["git_merge"])
            .filter(|m| m["result"] == "conflict")
        else {
            checked += 1;
            continue;
        };
        let listed = strings(&merge["conflicted_paths"]);
        if listed.contains(&miss.path) {
            checked += 1;
            hit += 1;
        } else if merge["conflicted_paths_total"].as_u64() == Some(listed.len() as u64) {
            checked += 1;
        }
    }
    Ok((hit, checked))
}

#[cfg(not(unix))]
fn merge_conflicts(
    _: Option<&Path>,
    _: &Path,
    _: &[NearMiss],
) -> std::result::Result<(usize, usize), String> {
    Err("not run on this platform".into())
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn apart(secs: i64) -> String {
    match secs / 60 {
        0 => "less than a minute apart".into(),
        minutes => format!("{minutes} min apart"),
    }
}

pub fn run(args: AutopsyArgs) -> Result<()> {
    let start = match args.repo {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let root = repo_root(&start).ok_or_else(|| {
        Error::Invalid(format!(
            "{} is not inside a Git repository",
            start.display()
        ))
    })?;
    // Absolute and resolved: a relative PATH entry must not run a `git` from this directory.
    let git = args
        .git
        .or_else(|| crate::quickstart::detect::which("git"))
        .and_then(|git| git.canonicalize().ok());
    let logs_dir = match args.logs {
        Some(dir) => dir,
        None => home()
            .ok_or_else(|| Error::Invalid("no home directory found; pass --logs".into()))?
            .join(".claude")
            .join("projects"),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let since = i64::try_from(now).unwrap_or(0) - i64::from(args.days) * DAY;
    let roots = repo_roots(&start, &root, git.as_deref());
    let logs = read_logs(&logs_dir, &roots, since)?;
    let unreadable = match logs.unreadable {
        0 => String::new(),
        n => format!(" ({n} unreadable skipped)"),
    };
    let read = format!(
        "Read: {}{unreadable} from {}",
        count(logs.files, "log file", "log files"),
        tilde(&logs_dir)
    );
    if logs.edits.is_empty() {
        println!(
            "No edits to files in this repository were found in the last {}.\n{read}\nRepository: {}",
            count(args.days as usize, "day", "days"),
            root.display()
        );
        return Ok(());
    }

    let replay = autopsy::replay(logs.edits);
    let near = &replay.near_misses;
    // File names come from the logs: no terminal escapes or bidirectional text in the output.
    let shown = |path: &str| {
        if args.no_paths {
            "<file>".to_owned()
        } else {
            sanitize(path, 200)
        }
    };
    let checked = if near.is_empty() {
        Ok((0, 0))
    } else {
        merge_conflicts(git.as_deref(), &root, near)
    };

    let mut card = vec![
        count(args.days as usize, "day", "days"),
        count(replay.sessions, "session", "sessions"),
        count(near.len(), "near-miss", "near-misses"),
    ];
    if let Some(worst) = autopsy::worst(near) {
        card.push(format!(
            "worst: {} ({} sessions, {})",
            shown(worst.path),
            worst.sessions,
            apart(worst.gap_secs)
        ));
    }
    card.push(match &checked {
        // Near-misses exist but none sat on two differing local branches: nothing was tested.
        Ok((_, 0)) if !near.is_empty() => "merge-tree: nothing to check".to_owned(),
        Ok((hit, _)) => format!("merge-tree conflicts today: {hit}"),
        Err(_) => "merge-tree: not checked".to_owned(),
    });
    println!("{}", card.join(" · "));
    if !near.is_empty() {
        for ranked in autopsy::ranked(near).into_iter().take(LISTED) {
            println!(
                "  {} — {}",
                shown(ranked.path),
                count(ranked.count, "near-miss", "near-misses")
            );
        }
        println!();
    }
    println!(
        "Near-miss = two of your sessions touched the same file while the other still held it \
         (the hub's {}-minute hold, renewed by activity, capped at {} minutes). It means the hub \
         would have warned, not that anything would have broken.",
        SESSION_TTL_SECONDS / 60,
        HOLD_CAP_SECONDS / 60
    );
    println!(
        "Reads Claude Code logs only; other agents are not included. Log contents and report data are not uploaded; configured product telemetry may record only a coarse autopsy feature outcome. File \
         contents in the logs are never kept or printed."
    );
    if !near.is_empty() {
        match checked {
            Ok((_, 0)) => println!(
                "merge-tree check: none of the near-misses were on two different local branches."
            ),
            Ok((hit, total)) => println!(
                "merge-tree check: {hit} of {} on branches that still exist and differ; it compares today's branch tips, not the files as they were then.",
                count(total, "near-miss", "near-misses")
            ),
            Err(reason) => println!("merge-tree check: {reason}"),
        }
    }
    println!("{read}\nRepository: {}", root.display());
    Ok(())
}
