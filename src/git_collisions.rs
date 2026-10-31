//! Read-only early collision warnings between in-flight Git work.
//!
//! In-flight work is a branch with commits beyond the base, or a worktree with
//! uncommitted changes. Two items that touch the same path are `same-file`; when
//! Git's own merge machinery reports a conflict between two branch tips they are
//! `conflict`. Same-file is not proof of conflict, and clones or branches that
//! were never fetched into this repository are invisible. Git runs through the
//! shared snapshot command layer (cleared environment, hooks and fsmonitor off, no
//! pathspec magic). Repository-configured clean filters (`git status`) and merge
//! drivers (`merge-tree`) still run as they would in the user's shell, so only
//! point this at repositories you trust. The only repository write is `merge-tree`
//! adding unreferenced objects.
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::git_snapshot::Git;
use crate::{Error, Result};

pub const PROFILE: &str = "agentscm-git-collisions/v1";

const OUTPUT: u64 = 16 * 1024 * 1024;
const MAX_ITEMS: usize = 200;
const MAX_SHARED_PATHS: usize = 50;
/// Past this many changed paths the squash check is skipped and the branch is scanned.
const MAX_EQUIVALENCE_PATHS: usize = 5_000;
/// `--no-lazy-fetch` (shared command layer) needs 2.43; `merge-tree --write-tree` needs 2.38.
const MIN_GIT: (u32, u32) = (2, 43);

#[derive(Clone, Debug)]
pub struct Request {
    /// Absolute path of the Git executable.
    pub git: PathBuf,
    /// A working directory of the repository.
    pub repository: PathBuf,
    pub base: Option<String>,
    /// Empty means every local and remote branch.
    pub refs: Vec<String>,
    pub worktrees: bool,
    /// With empty `refs`, skip branches whose tip was committed more than this many days ago.
    pub active_days: Option<u32>,
    /// With empty `refs`, also scan branches whose changes the base already has through a squash
    /// or rebase merge. Off by default; branches merged into the base by ancestry never appear.
    pub include_merged: bool,
    /// Stop at this instant and mark the report `truncated: "deadline"`.
    pub deadline: Option<Instant>,
    /// Stop adding collisions before the compact JSON report exceeds this many
    /// bytes, and mark it `truncated: "output"`.
    pub max_bytes: Option<usize>,
}

struct Item {
    id: String,
    kind: &'static str,
    /// Branch tip, or the worktree's `HEAD`.
    commit: String,
    refs: Vec<String>,
    worktree: Option<String>,
    paths: BTreeSet<String>,
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn nul_fields(out: &[u8]) -> impl Iterator<Item = String> + '_ {
    out.split(|b| *b == 0).filter(|f| !f.is_empty()).map(lossy)
}

/// Probed without the shared layer, whose `--no-lazy-fetch` is itself new Git.
fn git_version(exe: &Path) -> Result<String> {
    if !exe.is_absolute() {
        return Err(Error::Invalid(
            "git executable path must be absolute".into(),
        ));
    }
    let out = std::process::Command::new(exe)
        .arg("--version")
        .env_clear()
        .output()?;
    if !out.status.success() {
        return Err(Error::Invalid(format!(
            "{} --version failed",
            exe.display()
        )));
    }
    Ok(lossy(&out.stdout).trim_end().to_owned())
}

fn parse_version(text: &str) -> Option<(u32, u32)> {
    let mut parts = text.split_whitespace().nth(2)?.split('.');
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// Resolves a user-supplied name to a full commit ID; later calls use only IDs.
fn resolve(git: &Git, name: &str) -> Result<String> {
    if name.is_empty() || name.starts_with('-') {
        return Err(Error::Invalid(format!("invalid ref name {name:?}")));
    }
    let spec = format!("{name}^{{commit}}");
    let args = [
        "rev-parse",
        "--verify",
        "--quiet",
        "--end-of-options",
        &spec,
    ];
    git.text(&args)
        .map_err(|_| Error::Invalid(format!("{name} does not resolve to a commit")))
}

fn short(name: &str) -> &str {
    let name = name.strip_prefix("refs/").unwrap_or(name);
    name.strip_prefix("heads/")
        .or_else(|| name.strip_prefix("remotes/"))
        .unwrap_or(name)
}

enum Scanned {
    /// No commits beyond the base.
    NothingNew,
    /// Its changes are already in the base (see `already_in_base`).
    Merged,
    Item(Item),
}

/// The (patch ID, commit) pairs `git patch-id` finds in a diff or in `git log -p` output; the
/// commit is all zeros for a plain diff.
fn patch_ids(git: &Git, diff: Vec<u8>) -> Vec<(String, String)> {
    let out = git.run(&["patch-id", "--stable"], Some(diff), OUTPUT);
    let text = String::from_utf8_lossy(&out.unwrap_or_default()).into_owned();
    text.lines()
        .filter_map(|line| {
            let (id, commit) = line.split_once(' ')?;
            Some((id.to_owned(), commit.to_owned()))
        })
        .collect()
}

/// Base commits read per `git log -p`, so one call's output stays well under `OUTPUT`.
const PATCH_ID_BATCH: usize = 100;

/// The patch ID of each base commit (`None`: no diff), computed once per scan. Every branch's
/// squash check searches the base commits since its fork, and those ranges overlap, so hashing
/// them per branch made the check quadratic: minutes on a repository with dozens of branches.
#[derive(Default)]
struct BasePatchIds(HashMap<String, Option<String>>);

impl BasePatchIds {
    /// Whether a non-merge commit in `fork..base` has patch ID `id`; `None` when Git fails.
    fn contains(&mut self, git: &Git, fork: &str, base: &str, id: &str) -> Option<bool> {
        let range = format!("{fork}..{base}");
        let commits = git.text(&["rev-list", "--no-merges", &range, "--"]).ok()?;
        let commits: Vec<&str> = commits.lines().collect();
        let missing: Vec<&str> = commits
            .iter()
            .copied()
            .filter(|commit| !self.0.contains_key(*commit))
            .collect();
        for batch in missing.chunks(PATCH_ID_BATCH) {
            let input = batch.join("\n") + "\n";
            let args = [
                "log",
                "--no-walk=unsorted",
                "-p",
                "--no-renames",
                "--no-color",
                "--stdin",
            ];
            let log = git.run(&args, Some(input.into_bytes()), OUTPUT).ok()?;
            let found: HashMap<String, String> = patch_ids(git, log)
                .into_iter()
                .map(|(id, commit)| (commit, id))
                .collect();
            for commit in batch {
                self.0
                    .insert((*commit).to_owned(), found.get(*commit).cloned());
            }
        }
        Some(commits.iter().any(|commit| {
            self.0
                .get(*commit)
                .is_some_and(|got| got.as_deref() == Some(id))
        }))
    }
}

/// Whether `base` and `commit` hold every one of `paths` with the same content (or both lack
/// it). A path with a line break cannot be asked about, so it counts as different.
fn same_files(git: &Git, base: &str, commit: &str, paths: &BTreeSet<String>) -> bool {
    if paths.iter().any(|path| path.contains('\n')) {
        return false;
    }
    let ask: String = paths
        .iter()
        .map(|path| format!("{base}:{path}\n{commit}:{path}\n"))
        .collect();
    let Ok(out) = git.run(
        &["cat-file", "--batch-check"],
        Some(ask.into_bytes()),
        OUTPUT,
    ) else {
        return false;
    };
    // "<oid> <type> <size>", or "<name> missing".
    let state = |line: &str| match line.strip_suffix(" missing") {
        Some(_) => "missing".to_owned(),
        None => line.to_owned(),
    };
    let text = String::from_utf8_lossy(&out);
    let lines: Vec<String> = text.lines().map(state).collect();
    lines.len() == paths.len() * 2 && lines.chunks(2).all(|pair| pair[0] == pair[1])
}

/// Whether the base already has what a branch changed, by any of:
/// - every commit has a patch-equal commit there (rebase or cherry-pick merge);
/// - the base tip holds every file the branch changed exactly as the branch has it;
/// - one base commit made the branch's whole change (squash merge), even if later commits
///   touched the same files.
///
/// A failed check says no, which only costs a possible extra warning.
fn already_in_base(
    git: &Git,
    base: &str,
    commit: &str,
    paths: &BTreeSet<String>,
    base_ids: &mut BasePatchIds,
) -> bool {
    let cherry = git.text(&["cherry", base, commit]).unwrap_or_default();
    if cherry.lines().next().is_some() && cherry.lines().all(|line| line.starts_with('-')) {
        return true;
    }
    if paths.is_empty() {
        return true;
    }
    if paths.len() <= MAX_EQUIVALENCE_PATHS && same_files(git, base, commit, paths) {
        return true;
    }
    let Ok(fork) = git.text(&["merge-base", base, commit]) else {
        return false;
    };
    let Ok(mine) = git.run(
        &["diff", "--no-renames", "--no-color", &fork, commit, "--"],
        None,
        OUTPUT,
    ) else {
        return false;
    };
    patch_ids(git, mine)
        .first()
        .is_some_and(|(id, _)| base_ids.contains(git, &fork, base, id).unwrap_or(false))
}

fn ref_item(
    git: &Git,
    base: &str,
    commit: &str,
    refs: Vec<String>,
    skip_merged: bool,
    base_ids: &mut BasePatchIds,
    output: u64,
) -> Result<Scanned> {
    let range = format!("{base}..{commit}");
    if git.text(&["rev-list", "--count", &range, "--"])? == "0" {
        return Ok(Scanned::NothingNew);
    }
    let range = format!("{base}...{commit}");
    let args = ["diff", "--name-only", "-z", "--no-renames", &range, "--"];
    let paths: BTreeSet<String> = nul_fields(&git.run(&args, None, output)?).collect();
    if skip_merged && already_in_base(git, base, commit, &paths, base_ids) {
        return Ok(Scanned::Merged);
    }
    Ok(Scanned::Item(Item {
        id: short(&refs[0]).to_owned(),
        kind: "ref",
        commit: commit.to_owned(),
        refs,
        worktree: None,
        paths,
    }))
}

fn worktree_items(
    git: &Git,
    items: &mut Vec<Item>,
    skipped: &mut Vec<Value>,
    output: u64,
    exclude: Option<&Path>,
) -> Result<()> {
    let out = git.run(&["worktree", "list", "--porcelain", "-z"], None, output)?;
    // (path, HEAD, bare, prunable)
    let mut listed: Vec<(PathBuf, String, bool, bool)> = Vec::new();
    for field in out.split(|b| *b == 0) {
        if let Some(path) = field.strip_prefix(b"worktree ") {
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(path));
            listed.push((path, String::new(), false, false));
        } else if let Some(last) = listed.last_mut() {
            if let Some(head) = field.strip_prefix(b"HEAD ") {
                last.1 = lossy(head);
            }
            last.2 |= field == b"bare";
            last.3 |= field.starts_with(b"prunable");
        }
    }
    for (path, head, bare, prunable) in listed {
        // Past the cap the caller errors anyway; past the deadline every status would fail.
        if items.len() > MAX_ITEMS
            || git
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
        {
            break;
        }
        if bare {
            continue;
        }
        // Advisory scans need only other worktrees. Skip this checkout before
        // status can consume the deadline; the full collision scan passes None.
        if exclude.is_some_and(|exclude| {
            path.as_path() == exclude
                || path
                    .canonicalize()
                    .is_ok_and(|canonical| canonical == exclude)
        }) {
            continue;
        }
        let name = format!("worktree:{}", path.display());
        if prunable || !path.is_dir() {
            skipped.push(json!({"name": name, "reason": "worktree directory is missing"}));
            continue;
        }
        let inside = Git {
            exe: git.exe,
            repository: &path,
            deadline: git.deadline,
        };
        let args = [
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
        ];
        let paths = match inside.run(&args, None, output) {
            // Each record is "XY path"; paths are relative to the worktree root.
            Ok(out) => nul_fields(&out)
                .filter_map(|record| record.get(3..).map(str::to_owned))
                .collect::<BTreeSet<_>>(),
            Err(error) => {
                skipped.push(json!({"name": name, "reason": error.to_string()}));
                continue;
            }
        };
        if !paths.is_empty() {
            items.push(Item {
                id: name,
                kind: "worktree",
                commit: head,
                refs: Vec::new(),
                worktree: Some(path.to_string_lossy().into_owned()),
                paths,
            });
        }
    }
    Ok(())
}

/// Git's own verdict for two branch tips, or why there is none.
fn merge_verdict(git: &Git, a: &Item, b: &Item) -> Value {
    if a.kind != "ref" || b.kind != "ref" {
        return json!({"result": "not-evaluated",
            "reason": "uncommitted changes have no commit to merge"});
    }
    let args = [
        "merge-tree",
        "--write-tree",
        "--name-only",
        "-z",
        "--no-messages",
        "--end-of-options",
        &a.commit,
        &b.commit,
    ];
    match git.run_code(&args, None, OUTPUT, true) {
        Ok((0, _)) => json!({"result": "clean"}),
        Ok((_, out)) => {
            // First field is the merged tree ID; the rest are conflicted paths.
            let paths: BTreeSet<String> = nul_fields(&out).skip(1).collect();
            json!({"result": "conflict", "conflicted_paths_total": paths.len(),
                "conflicted_paths": paths.iter().take(MAX_SHARED_PATHS).collect::<Vec<_>>()})
        }
        Err(error) => json!({"result": "error", "reason": error.to_string()}),
    }
}

/// A failed check counts as "not an ancestor", which only costs a possible extra warning.
fn is_ancestor(git: &Git, a: &str, b: &str) -> bool {
    matches!(
        git.run_code(&["merge-base", "--is-ancestor", a, b], None, 1024, true),
        Ok((0, _))
    )
}

/// Paths both items change, plus a file one adds where the other has a directory of
/// that name (Git conflicts on those even though no path is equal).
fn overlap(a: &BTreeSet<String>, b: &BTreeSet<String>) -> BTreeSet<String> {
    let mut shared = BTreeSet::new();
    for (mine, theirs) in [(a, b), (b, a)] {
        for path in mine {
            if theirs.contains(path) {
                shared.insert(path.clone());
            }
            for (i, _) in path.match_indices('/') {
                if theirs.contains(&path[..i]) {
                    shared.insert(path[..i].to_owned());
                }
            }
        }
    }
    shared
}

/// Branches already merged into the base by ancestry, recent enough for the scan. The base
/// itself and remote `HEAD` pointers are not branches.
fn merged_refs(git: &Git, base: &str, active_days: Option<u32>) -> Result<usize> {
    let args = [
        "for-each-ref",
        "--merged",
        base,
        "--format=%(objectname) %(committerdate:unix) %(refname)",
        "refs/heads",
        "refs/remotes",
    ];
    let cutoff = active_days.map(|days| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        now.saturating_sub(u64::from(days) * 86_400)
    });
    Ok(git
        .text(&args)?
        .lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, ' ');
            let (oid, date, name) = (fields.next()?, fields.next()?, fields.next()?);
            let branch =
                oid != base && !(name.starts_with("refs/remotes/") && name.ends_with("/HEAD"));
            (branch && cutoff.is_none_or(|cutoff| date.parse::<u64>().unwrap_or(0) >= cutoff))
                .then_some(())
        })
        .count())
}

pub fn collisions(request: &Request) -> Result<Value> {
    let git = Git {
        exe: &request.git,
        repository: &request.repository,
        deadline: request.deadline,
    };
    let version = git_version(&request.git)?;
    match parse_version(&version) {
        Some(found) if found >= MIN_GIT => {}
        _ => {
            return Err(Error::Invalid(format!(
                "Git 2.43 or newer is required; found {version:?}"
            )));
        }
    }
    let (base_name, base) = match &request.base {
        Some(name) => (name.clone(), resolve(&git, name)?),
        None => ["origin/HEAD", "main"]
            .iter()
            .find_map(|name| Some((name.to_string(), resolve(&git, name).ok()?)))
            .ok_or_else(|| Error::Invalid("no origin/HEAD or main; pass --base".into()))?,
    };

    let mut named: Vec<(String, String)> = Vec::new();
    let mut inactive = 0;
    if request.refs.is_empty() {
        let args = [
            "for-each-ref",
            "--no-merged",
            &base,
            "--format=%(objectname) %(committerdate:unix) %(refname)",
            "refs/heads",
            "refs/remotes",
        ];
        let listing = git.text(&args)?;
        let cutoff = request.active_days.map(|days| {
            let now = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |since| since.as_secs());
            now.saturating_sub(u64::from(days) * 86_400)
        });
        for line in listing.lines() {
            let mut fields = line.splitn(3, ' ');
            if let (Some(oid), Some(date), Some(name)) =
                (fields.next(), fields.next(), fields.next())
                && !(name.starts_with("refs/remotes/") && name.ends_with("/HEAD"))
            {
                if cutoff.is_some_and(|cutoff| date.parse::<u64>().unwrap_or(0) < cutoff) {
                    inactive += 1;
                } else {
                    named.push((oid.to_owned(), name.to_owned()));
                }
            }
        }
    } else {
        for name in &request.refs {
            named.push((resolve(&git, name)?, name.clone()));
        }
    }
    let mut by_commit: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (commit, name) in named {
        by_commit.entry(commit).or_default().push(name);
    }

    if by_commit.len() > MAX_ITEMS {
        return Err(Error::Limit(format!(
            "{} refs with commits beyond the base exceed {MAX_ITEMS}; narrow with --refs or --active-days",
            by_commit.len()
        )));
    }
    let (mut items, mut skipped) = (Vec::new(), Vec::new());
    let skip_merged = request.refs.is_empty() && !request.include_merged;
    // Refs the default scan leaves out because the base already has their changes.
    let mut merged = if skip_merged {
        merged_refs(&git, &base, request.active_days)?
    } else {
        0
    };
    let mut base_ids = BasePatchIds::default();
    for (commit, refs) in by_commit {
        let name = refs[0].clone();
        let count = refs.len();
        match ref_item(
            &git,
            &base,
            &commit,
            refs,
            skip_merged,
            &mut base_ids,
            OUTPUT,
        ) {
            Ok(Scanned::Item(item)) => items.push(item),
            Ok(Scanned::Merged) => merged += count,
            Ok(Scanned::NothingNew) => {}
            Err(error) => skipped.push(json!({"name": name, "reason": error.to_string()})),
        }
    }
    let past = || {
        request
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
    };
    if request.worktrees && !past() {
        worktree_items(&git, &mut items, &mut skipped, OUTPUT, None)?;
    }
    items.sort_by(|a, b| a.id.cmp(&b.id));
    if items.len() > MAX_ITEMS {
        return Err(Error::Limit(format!(
            "{} in-flight items exceed {MAX_ITEMS}; narrow with --refs",
            items.len()
        )));
    }

    let items_json: Vec<Value> = items
        .iter()
        .map(|item| {
            json!({
                "id": item.id, "kind": item.kind, "commit": item.commit,
                "refs": item.refs, "worktree": item.worktree, "paths_total": item.paths.len(),
            })
        })
        .collect();
    // Everything else in the report is small and fixed; callers keep headroom for it.
    let mut bytes = json!(items_json).to_string().len() + json!(skipped).to_string().len();
    let mut truncated = None;
    let mut found = Vec::new();
    'pairs: for (i, a) in items.iter().enumerate() {
        for b in &items[i + 1..] {
            if past() {
                break 'pairs;
            }
            let shared: Vec<String> = overlap(&a.paths, &b.paths).into_iter().collect();
            if shared.is_empty() {
                continue;
            }
            // Stacked branches, and a worktree on or above a branch, are one line of work.
            let one_line = match (a.kind, b.kind) {
                ("ref", "ref") => {
                    is_ancestor(&git, &a.commit, &b.commit)
                        || is_ancestor(&git, &b.commit, &a.commit)
                }
                ("ref", _) => is_ancestor(&git, &a.commit, &b.commit),
                (_, "ref") => is_ancestor(&git, &b.commit, &a.commit),
                _ => false,
            };
            if one_line {
                continue;
            }
            let verdict = merge_verdict(&git, a, b);
            let level = if verdict["result"] == "conflict" {
                "conflict"
            } else {
                "same-file"
            };
            let entry = json!({
                "level": level,
                "items": [a.id, b.id],
                "shared_paths_total": shared.len(),
                "shared_paths": shared.iter().take(MAX_SHARED_PATHS).collect::<Vec<_>>(),
                "git_merge": verdict,
            });
            bytes += entry.to_string().len() + 1;
            if request.max_bytes.is_some_and(|max| bytes > max) {
                truncated = Some("output");
                break 'pairs;
            }
            found.push(entry);
        }
    }
    // Also covers a deadline hit while listing items, whose misses are in `skipped`.
    if past() {
        truncated = Some("deadline");
    }
    found.sort_by_key(|c| {
        (
            c["level"] != "conflict",
            std::cmp::Reverse(c["shared_paths_total"].as_u64()),
        )
    });

    Ok(json!({
        "profile": PROFILE,
        "git_version": version,
        "base": {"name": base_name, "commit": base},
        "active_days": request.refs.is_empty().then_some(request.active_days).flatten(),
        "inactive_refs_excluded": inactive,
        "merged_refs_excluded": merged,
        "items": items_json,
        "collisions": found,
        "truncated": truncated,
        "skipped": skipped,
        "notes": [
            "same-file means two items changed the same path; it is not proof that Git would conflict.",
            "conflict means git merge-tree reported a conflict between two branch tips; uncommitted changes are never evaluated for conflicts.",
            "Only branches and worktrees visible to this repository are checked; clones, branches or uncommitted work elsewhere are invisible.",
        ],
    }))
}

/// A deliberately incomplete, path-only observation for command hooks. It never runs
/// merge-tree, patch-id, or fetch. Exact commits describe the observation, not current refs.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvisorySnapshot {
    pub base_name: String,
    pub base_commit: String,
    pub head: String,
    pub observed_at: u64,
    pub peers: Vec<AdvisoryPeer>,
    pub incomplete: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdvisoryPeer {
    /// Exact unique merge base used to select a branch's changed paths.
    pub fork: Option<String>,
    pub name: String,
    pub commit: String,
    pub kind: String,
    pub paths: BTreeSet<String>,
}

/// These limits also bound cache parsing and first-edit lookup.
pub const ADVISORY_BYTES: usize = 256 * 1024;
const ADVISORY_ITEMS: usize = 40;

/// Hook paths are UTF-8, relative, normalized and free of display controls. Reject
/// lossy Git names here rather than confusing two distinct paths in an advisory.
pub fn advisory_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 1024
        && !path.contains('\\')
        && !path.chars().any(|c| c.is_control() || c == '\u{fffd}')
        && path.split('/').all(|part| !matches!(part, "" | "." | ".."))
}

impl AdvisorySnapshot {
    /// A cache is only a bounded historical observation. There is no negative
    /// assurance: unseen branches, later commits and later dirty work are unknown.
    pub fn valid(&self, now: u64) -> bool {
        let oid = |value: &str| {
            matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
        };
        self.observed_at <= now
            && now - self.observed_at <= 4 * 60 * 60
            && oid(&self.base_commit)
            && oid(&self.head)
            && matches!(self.base_name.as_str(), "origin/HEAD" | "main")
            && self.peers.len() <= ADVISORY_ITEMS
            && self.peers.iter().all(|peer| {
                oid(&peer.commit)
                    && match peer.kind.as_str() {
                        "ref" => peer.fork.as_deref().is_some_and(oid),
                        "worktree" => peer.fork.is_none(),
                        _ => false,
                    }
                    && peer.name.len() <= 1024
                    && peer.paths.iter().all(|path| advisory_path(path))
            })
    }

    pub fn touching(&self, path: &str) -> Vec<&AdvisoryPeer> {
        if !advisory_path(path) {
            return Vec::new();
        }
        let mine = BTreeSet::from([path.to_owned()]);
        self.peers
            .iter()
            .filter(|peer| !overlap(&mine, &peer.paths).is_empty())
            .take(3)
            .collect()
    }
}

/// Bounded session-start variant of the existing detector. Unlike the full
/// collision report, it compares future edit paths with each peer's changed paths,
/// so pairwise merge simulation would be both wasteful and misleading.
pub fn advisory_snapshot(
    git_path: &Path,
    repository: &Path,
    deadline: Instant,
) -> Result<AdvisorySnapshot> {
    let git = Git {
        exe: git_path,
        repository,
        deadline: Some(deadline),
    };
    let version = git.text(&["--version"])?;
    if parse_version(&version).is_none_or(|found| found < MIN_GIT) {
        return Err(Error::Invalid("Git 2.43 or newer is required".into()));
    }
    let head = resolve(&git, "HEAD")?;
    let (base_name, base_commit) = if let Ok(base) = resolve(&git, "origin/HEAD") {
        ("origin/HEAD".to_owned(), base)
    } else {
        let main = resolve(&git, "main")?;
        if resolve(&git, "master").is_ok_and(|master| master != main) {
            return Err(Error::Invalid(
                "ambiguous default base; configure origin/HEAD".into(),
            ));
        }
        ("main".to_owned(), main)
    };
    let repository = repository.canonicalize()?;
    let mut items = Vec::new();
    let mut skipped = Vec::new();
    // Dirty linked worktrees are the strongest local signal. Scan and retain
    // those before branches consume the shared deadline or peer budget.
    let mut incomplete = worktree_items(
        &git,
        &mut items,
        &mut skipped,
        ADVISORY_BYTES as u64,
        Some(&repository),
    )
    .is_err();
    let listing = git.run(
        &[
            "for-each-ref",
            "--sort=refname",
            "--sort=-committerdate",
            "--count=41",
            "--no-merged",
            &base_commit,
            "--format=%(objectname) %(refname)",
            "refs/heads",
            "refs/remotes",
        ],
        None,
        ADVISORY_BYTES as u64,
    );
    let mut named: Vec<(String, Vec<String>)> = Vec::new();
    let mut positions = HashMap::new();
    match listing {
        Ok(listing) => {
            let listing = lossy(&listing);
            incomplete |= listing.lines().count() > ADVISORY_ITEMS;
            for line in listing.lines().take(ADVISORY_ITEMS) {
                if let Some((commit, name)) = line.split_once(' ')
                    && !(name.starts_with("refs/remotes/") && name.ends_with("/HEAD"))
                {
                    if let Some(&index) = positions.get(commit) {
                        let (_, refs): &mut (String, Vec<String>) = &mut named[index];
                        refs.push(name.to_owned());
                    } else {
                        positions.insert(commit.to_owned(), named.len());
                        named.push((commit.to_owned(), vec![name.to_owned()]));
                    }
                }
            }
        }
        Err(_) => incomplete = true,
    }
    let mut ids = BasePatchIds::default();
    let mut forks = BTreeMap::new();
    for (commit, refs) in named {
        if items.len() >= ADVISORY_ITEMS {
            incomplete = true;
            break;
        }
        // A peer already contained in this checkout cannot introduce new work.
        if is_ancestor(&git, &commit, &head) {
            continue;
        }
        let fork = match git.text(&["merge-base", "--all", &base_commit, &commit]) {
            Ok(fork) if fork.lines().count() == 1 => fork,
            _ => {
                incomplete = true;
                continue;
            }
        };
        forks.insert(commit.clone(), fork.clone());
        match ref_item(
            &git,
            &fork,
            &commit,
            refs,
            false,
            &mut ids,
            ADVISORY_BYTES as u64,
        ) {
            Ok(Scanned::Item(item)) => items.push(item),
            Ok(_) => {}
            Err(_) => incomplete = true,
        }
        if Instant::now() >= deadline {
            incomplete = true;
            break;
        }
    }
    incomplete |= !skipped.is_empty() || Instant::now() >= deadline;
    let mut peers = Vec::new();
    for item in items {
        if peers.len() >= ADVISORY_ITEMS {
            incomplete = true;
            break;
        }
        if !item.paths.iter().all(|path| advisory_path(path)) {
            incomplete = true;
        }
        let peer = AdvisoryPeer {
            fork: (item.kind == "ref").then(|| forks[&item.commit].clone()),
            name: item.id,
            commit: item.commit,
            kind: item.kind.to_owned(),
            paths: item
                .paths
                .into_iter()
                .filter(|path| advisory_path(path))
                .collect(),
        };
        peers.push(peer);
        if serde_json::to_vec(&peers)?.len() > ADVISORY_BYTES / 2 {
            peers.pop();
            incomplete = true;
            break;
        }
    }
    Ok(AdvisorySnapshot {
        base_name,
        base_commit,
        head,
        observed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |time| time.as_secs()),
        peers,
        incomplete,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_versions() {
        assert_eq!(
            parse_version("git version 2.50.1 (Apple Git-155)"),
            Some((2, 50))
        );
        assert_eq!(parse_version("git version 2.43.0"), Some((2, 43)));
        assert_eq!(parse_version("nonsense"), None);
    }

    #[test]
    fn shortens_ref_names() {
        assert_eq!(short("refs/heads/a/b"), "a/b");
        assert_eq!(short("refs/remotes/origin/a"), "origin/a");
        assert_eq!(short("HEAD"), "HEAD");
    }
}
