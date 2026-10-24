use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::IsTerminal;
use std::path::PathBuf;

use agentscm::Result;
use agentscm::git_collisions::{self, Request};
use agentscm::hub::advice::sanitize;
use clap::Args;
use serde_json::{Value, json};

const MAX_PAIRS: usize = 10;
const MAX_FILES: usize = 5;
const MAX_PATHS: usize = 3;

#[derive(Debug, Args)]
pub struct CollisionsArgs {
    /// Absolute path of the Git executable. Defaults to the first git on PATH, then /usr/bin/git.
    #[arg(long)]
    git: Option<PathBuf>,
    /// A working directory of the Git repository to inspect. Defaults to the current directory.
    #[arg(long, default_value = ".")]
    git_dir: PathBuf,
    /// Compare against this ref. Defaults to origin/HEAD, then main.
    #[arg(long)]
    base: Option<String>,
    /// Check only this ref. Repeatable. Defaults to all local and remote branches.
    #[arg(long = "refs", num_args = 1.., value_name = "REF")]
    refs: Vec<String>,
    /// Also check every worktree's uncommitted and untracked changes.
    #[arg(long)]
    worktrees: bool,
    /// Without --refs, skip branches with no commit in this many days. 0 checks all.
    #[arg(long, default_value_t = 14)]
    active_days: u32,
    /// Also scan branches whose changes the default branch already has (squash or rebase merged).
    #[arg(long)]
    include_merged: bool,
    /// Print the full JSON report. This is the default when stdout is not a terminal.
    #[arg(long, conflicts_with = "text")]
    json: bool,
    /// Print the short text report even when stdout is not a terminal.
    #[arg(long)]
    text: bool,
}

pub fn run(args: CollisionsArgs) -> Result<()> {
    let text = !args.json && (args.text || std::io::stdout().is_terminal());
    let report = git_collisions::collisions(&Request {
        git: args.git.unwrap_or_else(default_git),
        repository: args.git_dir,
        base: args.base,
        refs: args.refs,
        worktrees: args.worktrees,
        active_days: (args.active_days > 0).then_some(args.active_days),
        include_merged: args.include_merged,
        deadline: None,
        max_bytes: None,
    });
    let report = match report {
        Ok(report) => report,
        // Same exit code as the JSON error path in main.
        Err(error) if text => {
            eprintln!("agentscm: collisions: {}", clean(&error.to_string()));
            std::process::exit(1);
        }
        Err(error) => return Err(error),
    };
    if text {
        print!("{}", render(&report, args.worktrees));
    } else {
        println!("{}", json!({"ok": true, "collisions": report}));
    }
    Ok(())
}

/// The first `git` on PATH as an absolute path, else /usr/bin/git.
fn default_git() -> PathBuf {
    crate::quickstart::detect::which("git")
        .and_then(|git| git.canonicalize().ok())
        .unwrap_or_else(|| PathBuf::from("/usr/bin/git"))
}

/// Ref and file names may hold terminal escapes; drop every control character.
fn clean(text: &str) -> String {
    sanitize(text, 300)
}

fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn name(value: &Value) -> String {
    clean(value.as_str().unwrap_or("?"))
}

fn pair_lines(out: &mut String, pairs: &[&Value]) {
    for pair in pairs.iter().take(MAX_PAIRS) {
        // A pair that conflicts shows the files that conflict, not every file both touched.
        let conflicted = &pair["git_merge"]["conflicted_paths"];
        let (list, total) = if pair["level"] == "conflict" && conflicted.is_array() {
            (conflicted, &pair["git_merge"]["conflicted_paths_total"])
        } else {
            (&pair["shared_paths"], &pair["shared_paths_total"])
        };
        let paths: Vec<String> = list
            .as_array()
            .map_or(&[][..], Vec::as_slice)
            .iter()
            .take(MAX_PATHS)
            .map(name)
            .collect();
        let total = total.as_u64().unwrap_or(0) as usize;
        let mut line = paths.join(", ");
        if total > paths.len() {
            let _ = write!(line, " (+{} more)", total - paths.len());
        }
        let _ = writeln!(
            out,
            "  {}  ↔  {}   {line}",
            name(&pair["items"][0]),
            name(&pair["items"][1])
        );
    }
    if pairs.len() > MAX_PAIRS {
        let _ = writeln!(out, "  ... and {} more", pairs.len() - MAX_PAIRS);
    }
}

fn render(report: &Value, worktrees: bool) -> String {
    let all: Vec<&Value> = report["collisions"]
        .as_array()
        .into_iter()
        .flatten()
        .collect();
    let (conflicts, same): (Vec<&Value>, Vec<&Value>) =
        all.iter().partition(|c| c["level"] == "conflict");
    let checked = report["items"].as_array().map_or(0, Vec::len);
    let mut out = format!(
        "Checked {} against {}",
        count(checked, "branch/worktree", "branches/worktrees"),
        name(&report["base"]["name"])
    );
    if all.is_empty() {
        out.push_str(" · No colliding branches found");
    } else {
        let _ = write!(
            out,
            " · {} the same files",
            if all.len() == 1 {
                "1 pair touches".to_owned()
            } else {
                format!("{} pairs touch", all.len())
            }
        );
        if !conflicts.is_empty() {
            let _ = write!(out, " · {} would conflict in git", conflicts.len());
        }
    }
    out.push('\n');
    for (title, pairs) in [
        ("Would conflict (git merge-tree):", &conflicts),
        ("Touch the same files (git can still merge them):", &same),
    ] {
        if !pairs.is_empty() {
            let _ = writeln!(out, "\n{title}");
            pair_lines(&mut out, pairs);
        }
    }

    // Counts only the shared paths listed per pair, which the report caps.
    let mut files: BTreeMap<String, usize> = BTreeMap::new();
    for pair in &all {
        for path in pair["shared_paths"].as_array().into_iter().flatten() {
            *files.entry(name(path)).or_default() += 1;
        }
    }
    let mut files: Vec<_> = files.into_iter().filter(|(_, n)| *n > 1).collect();
    files.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if !files.is_empty() {
        out.push_str("\nMost contested files:\n");
        for (path, n) in files.iter().take(MAX_FILES) {
            let _ = writeln!(out, "  {path} — {}", count(*n, "pair", "pairs"));
        }
    }

    let mut notes = Vec::new();
    if let Some(why) = report["truncated"].as_str() {
        notes.push(format!("Stopped early: {}; partial result.", clean(why)));
    }
    let skipped = report["skipped"].as_array().map_or(0, Vec::len);
    if skipped > 0 {
        notes.push(format!(
            "{} could not be checked.",
            count(skipped, "ref or worktree", "refs or worktrees")
        ));
    }
    let merged = report["merged_refs_excluded"].as_u64().unwrap_or(0) as usize;
    if merged > 0 {
        notes.push(format!(
            "{} skipped: already merged (--include-merged adds squash and rebase merges).",
            count(merged, "branch", "branches")
        ));
    }
    let inactive = report["inactive_refs_excluded"].as_u64().unwrap_or(0) as usize;
    if inactive > 0 {
        notes.push(format!(
            "{} skipped as inactive (--active-days 0 checks all).",
            count(inactive, "branch", "branches")
        ));
    }
    if !all.is_empty() {
        notes.push("Same file is a warning, not proof of conflict.".into());
    }
    notes.push(if worktrees {
        "Uncommitted changes are never checked for conflicts. Add --json for the full report.".into()
    } else {
        "Uncommitted changes are never checked for conflicts. Add --worktrees to include uncommitted work, --json for the full report.".into()
    });
    let _ = write!(out, "\n{}\n", notes.join(" "));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(level: &str, a: &str, b: &str, paths: &[&str], total: usize) -> Value {
        // A conflict names its own files: here only the first of the shared ones.
        let merge = if level == "conflict" {
            json!({"result": "conflict", "conflicted_paths": &paths[..1], "conflicted_paths_total": 1})
        } else {
            json!({"result": "clean"})
        };
        json!({"level": level, "items": [a, b], "shared_paths_total": total,
               "shared_paths": paths, "git_merge": merge})
    }

    fn report(collisions: Vec<Value>) -> Value {
        json!({"base": {"name": "origin/main", "commit": "c"},
               "items": [{"id": "a"}, {"id": "b"}, {"id": "c"}],
               "collisions": collisions, "truncated": null, "skipped": [],
               "inactive_refs_excluded": 0, "merged_refs_excluded": 0, "notes": []})
    }

    #[test]
    fn clean_drops_control_characters() {
        assert_eq!(clean("a\x1b[31mb\nc\u{7}"), "a[31mbc");
    }

    #[test]
    fn renders_conflict_same_file_and_contested_files() {
        let text = render(
            &report(vec![
                pair(
                    "conflict",
                    "feat-x",
                    "feat-y",
                    &["a.rs", "b.rs", "c.rs", "d.rs"],
                    7,
                ),
                pair("same-file", "feat-x", "feat-z", &["a.rs"], 1),
            ]),
            false,
        );
        assert!(text.starts_with(
            "Checked 3 branches/worktrees against origin/main · 2 pairs touch the same files · 1 would conflict in git\n"
        ));
        assert!(text.contains("Would conflict (git merge-tree):\n  feat-x  ↔  feat-y   a.rs\n"));
        assert!(text.contains(
            "Touch the same files (git can still merge them):\n  feat-x  ↔  feat-z   a.rs\n"
        ));
        assert!(text.contains("Most contested files:\n  a.rs — 2 pairs\n"));
        assert!(text.contains("Add --worktrees"));
        assert!(!render(&report(vec![]), true).contains("Add --worktrees"));
    }

    #[test]
    fn says_how_many_merged_branches_were_skipped() {
        let mut r = report(vec![]);
        r["merged_refs_excluded"] = json!(41);
        assert!(render(&r, false).contains(
            "41 branches skipped: already merged (--include-merged adds squash and rebase merges)."
        ));
        r["merged_refs_excluded"] = json!(1);
        assert!(render(&r, false).contains("1 branch skipped: already merged"));
        assert!(!render(&report(vec![]), false).contains("already merged"));
    }

    #[test]
    fn renders_no_pairs_truncation_and_skips() {
        let mut r = report(vec![]);
        r["truncated"] = json!("deadline");
        r["skipped"] = json!([{"name": "x", "reason": "y"}]);
        let text = render(&r, false);
        assert!(text.starts_with(
            "Checked 3 branches/worktrees against origin/main · No colliding branches found\n"
        ));
        assert!(text.contains("Stopped early: deadline; partial result."));
        assert!(text.contains("1 ref or worktree could not be checked."));
        assert!(!text.contains("Most contested"));
    }

    #[test]
    fn single_pair_and_limits_and_control_characters() {
        let long = render(
            &report(vec![pair("same-file", "a", "b", &["1", "2", "3", "4"], 7)]),
            false,
        );
        assert!(long.contains("a  ↔  b   1, 2, 3 (+4 more)\n"), "{long}");
        let one = render(&report(vec![pair("same-file", "a", "b", &["f"], 1)]), false);
        assert!(one.contains("1 pair touches the same files\n"));
        let many: Vec<Value> = (0..12)
            .map(|i| pair("same-file", &format!("a{i}"), "b\x1b[2J", &["f\x07"], 1))
            .collect();
        let text = render(&report(many), false);
        assert_eq!(text.matches('↔').count(), MAX_PAIRS);
        assert!(text.contains("... and 2 more"));
        assert!(text.contains("f — 12 pairs"));
        assert!(text.contains("b[2J"));
        assert!(!text.chars().any(|c| c.is_control() && c != '\n'));
    }
}
