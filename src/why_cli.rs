//! `agentscm why <commit | path[:line]>`: the Work the Hub recorded for a commit or a file.
//! Local Git turns a line into the commit that last changed it (`git blame`) and a path into its
//! recent commits (`git log`); the Hub answers for each commit and for the path's declared scope.
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use agentscm::{Error, Result};
use clap::Args;
use reqwest::Method;
use serde_json::{Value, json};

use crate::intent_cli::Hub;

const LIMIT: Duration = Duration::from_secs(10);
const NO_COMMIT: &str = "0000000000000000000000000000000000000000";

#[derive(Debug, Args)]
pub struct WhyArgs {
    /// A commit Git can resolve, a repository path, or path:line.
    target: String,
    /// For a path: how many of its newest commits to ask about.
    #[arg(long, default_value_t = 5, value_parser = clap::value_parser!(u8).range(1..=20))]
    commits: u8,
}

/// What to ask the Hub: commits, and the path whose scope to match.
struct Subjects {
    commits: Vec<String>,
    path: Option<String>,
}

pub async fn run(args: WhyArgs) -> Result<Value> {
    let subjects = subjects(&args.target, args.commits)?;
    let hub = Hub::connect(LIMIT)?;
    let mut answers = Vec::new();
    for commit in &subjects.commits {
        answers.push(ask(&hub, "commit", commit).await?);
    }
    if let Some(path) = &subjects.path {
        answers.push(ask(&hub, "path", path).await?);
    }
    Ok(
        json!({"target": args.target, "commits": subjects.commits, "path": subjects.path,
        "answers": answers}),
    )
}

async fn ask(hub: &Hub, key: &str, value: &str) -> Result<Value> {
    let mut url = reqwest::Url::parse("http://hub/v1/hub/standup/why")
        .map_err(|_| invalid("cannot construct why URL"))?;
    url.query_pairs_mut().append_pair(key, value);
    let path = &url.as_str()["http://hub".len()..];
    hub.call(Method::GET, path, None).await
}

fn subjects(target: &str, commits: u8) -> Result<Subjects> {
    if let Some((path, line)) = target.rsplit_once(':')
        && let Ok(line) = line.parse::<u32>()
        && line > 0
        && Path::new(path).is_file()
    {
        let blame = git(&[
            "blame",
            "--porcelain",
            "-L",
            &format!("{line},{line}"),
            "--",
            path,
        ])?;
        let commit = blame.split_whitespace().next().unwrap_or_default();
        return Ok(Subjects {
            // An uncommitted line has no commit to ask about.
            commits: with_merges((commit != NO_COMMIT).then(|| commit.to_owned()))?,
            path: repository_path(path)?,
        });
    }
    if Path::new(target).exists() {
        let log = git(&[
            "log",
            &format!("--max-count={commits}"),
            "--format=%H",
            "--",
            target,
        ])?;
        return Ok(Subjects {
            commits: with_merges(log.lines().map(str::to_owned))?,
            path: repository_path(target)?,
        });
    }
    let commit = git(&[
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        &format!("{target}^{{commit}}"),
    ])
    .ok()
    .map(|commit| commit.trim().to_owned())
    .filter(|commit| full_sha(commit))
    .ok_or_else(|| invalid("target is neither a commit nor a path in this checkout"))?;
    Ok(Subjects {
        commits: with_merges(Some(commit))?,
        path: None,
    })
}

fn full_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Each commit, then the merge on `HEAD`'s first-parent line that brought it in, if any: the Hub
/// knows a pull request by its merge and head commits, rarely by a commit inside the branch, and
/// a merge of `main` into the branch is not one. A squash-merged commit is already the merge.
fn with_merges(commits: impl IntoIterator<Item = String>) -> Result<Vec<String>> {
    let mut all = Vec::new();
    for commit in commits {
        let range = format!("{commit}..HEAD");
        let descendants = git(&["log", "--ancestry-path", "--format=%H", &range])?;
        let descendants: std::collections::HashSet<&str> = descendants.lines().collect();
        let line = git(&[
            "log",
            "--first-parent",
            "--merges",
            "--reverse",
            "--format=%H",
            &range,
        ])?;
        let merge = line.lines().find(|merge| descendants.contains(merge));
        for commit in std::iter::once(commit.clone()).chain(merge.map(str::to_owned)) {
            if !all.contains(&commit) {
                all.push(commit);
            }
        }
    }
    Ok(all)
}

/// `path` relative to the repository root, a directory ending in `/`; `None` for the root itself.
fn repository_path(path: &str) -> Result<Option<String>> {
    let root = std::fs::canonicalize(git(&["rev-parse", "--show-toplevel"])?.trim_end())?;
    let full = std::fs::canonicalize(path)?;
    let relative = full
        .strip_prefix(&root)
        .map_err(|_| invalid("path is outside this repository"))?
        .to_str()
        .ok_or_else(|| invalid("path is not UTF-8"))?
        .replace(std::path::MAIN_SEPARATOR, "/");
    Ok((!relative.is_empty()).then(|| {
        if full.is_dir() {
            format!("{relative}/")
        } else {
            relative
        }
    }))
}

fn git(args: &[&str]) -> Result<String> {
    let output = Command::new("git").args(args).output()?;
    if !output.status.success() {
        return Err(invalid(&format!(
            "git {} failed: {}",
            args[0],
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    String::from_utf8(output.stdout).map_err(|_| invalid("git output is not UTF-8"))
}

fn invalid(message: &str) -> Error {
    Error::Invalid(message.to_owned())
}
