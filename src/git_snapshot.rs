//! Bounded, read-only preflight of one committed Git tree for native snapshotting.
//!
//! Git is a trusted operator-selected executable run with a cleared environment,
//! no hooks, no replace objects, no lazy fetch and no pathspec magic. Only the
//! committed tree is read; untracked or ignored files are never captured. This
//! module does not claim to detect secrets, publish revisions or run candidates.
use std::collections::BTreeSet;
use std::io::Write;
use std::ops::Range;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ring::digest::{Context, SHA1_FOR_LEGACY_USE_ONLY};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::execution::Cancellation;
use crate::execution::process::capture_with_input;
use crate::git_path_profile::{PortableSpellings, uses_reserved_namespace};
use crate::model::{EntryChange, EntryKind};
use crate::store::{
    Engine, MAX_CHANGES, MAX_CHECKPOINT_BYTES, MAX_CHECKPOINT_PREFIXES, MAX_FILE_BYTES,
    MAX_SYMLINK_TARGET_BYTES, portable_path, validate_path, validate_symlink_target,
};
use crate::{Error, Result};

/// Profile of a capture whose entries are all regular files (mode `100644`).
pub const PROFILE: &str = "agentscm-git-snapshot/v1";
/// Profile of a capture that holds at least one `100755` or `120000` entry.
pub const PROFILE_V2: &str = "agentscm-git-snapshot/v2";

const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const MINIMUM_GIT_VERSION: &str = "2.45.0";
const VERSION_OUTPUT: u64 = 4096;
const SMALL_OUTPUT: u64 = 1024 * 1024;
const STATUS_OUTPUT: u64 = 16 * 1024 * 1024;
const TREE_OUTPUT: u64 = 64 * 1024 * 1024;
const IGNORED_OUTPUT: u64 = 16 * 1024 * 1024;
const STDERR_EXCERPT: usize = 512;
const MAX_REPORTED_PATHS: usize = 20;
// Same bounds as the local workspace (src/workspace.rs), which are the stricter ones.
const MAX_ENTRIES: usize = 100_000;
const MAX_DEPTH: usize = 128;
const BLOB_CHUNK_BYTES: u64 = 16 * 1024 * 1024;
const BLOB_CHUNK_FILES: usize = 1_024;
const BLOB_HEADER_ALLOWANCE: u64 = 160;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRequest {
    /// Absolute path of the Git executable.
    pub git: PathBuf,
    pub repository: PathBuf,
    /// Full lowercase hexadecimal commit object ID.
    pub revision: String,
    /// Tracked symlink or gitlink paths the operator declares as not captured.
    /// A symlink not listed here is captured as a symlink entry.
    pub exclude: Vec<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: String,
    pub mode: String,
    pub oid: String,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedKind {
    Gitlink,
    /// A `120000` entry whose target is not an in-tree relative canonical path.
    SymlinkTarget,
    UnknownMode,
    NonPortablePath,
    CaseAlias,
    FileDirectoryConflict,
    ReservedNamespace,
    IgnoredTracked,
    InvalidExclusion,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Unsupported {
    pub kind: UnsupportedKind,
    pub path: String,
    /// Reason, or the other path for a case alias.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    FileBytes,
    TotalBytes,
    Entries,
    Depth,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct LimitViolation {
    pub limit: LimitKind,
    pub observed: u64,
    pub maximum: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Cleanliness {
    /// Commit `HEAD` points at, or `(initial)`.
    pub head: String,
    /// `HEAD` equals the selected commit and no tracked path differs.
    pub clean: bool,
    pub tracked_changes: u64,
    /// At most the first 20 changed paths, lossily decoded.
    pub tracked_change_paths: Vec<String>,
    /// Untracked, non-ignored files: reported, allowed, never captured.
    pub untracked_count: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Totals {
    pub files: u64,
    pub directories: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Preflight {
    pub profile: &'static str,
    pub git_version: String,
    pub repository: String,
    /// Absolute common Git directory (holds config, objects and linked worktree data).
    pub git_dir: String,
    pub object_format: String,
    pub shallow: bool,
    pub commit: String,
    pub tree: String,
    pub cleanliness: Cleanliness,
    /// Configured `filter.*` keys; any makes the checkout unverifiable.
    pub filters: Vec<String>,
    pub entries: Vec<ManifestEntry>,
    pub excluded: Vec<ManifestEntry>,
    pub unsupported: Vec<Unsupported>,
    pub totals: Totals,
    pub limit_violations: Vec<LimitViolation>,
    /// SHA-256 (lowercase hex) over `entries` and `excluded` merged in path-byte
    /// order, each as `"{mode} {oid} {size} {path}\0"`.
    pub manifest_sha256: String,
    pub eligible: bool,
}

fn git_command(git: &Path, repository: &Path) -> Result<Command> {
    if !git.is_absolute() {
        return Err(Error::Invalid(
            "git executable path must be absolute".into(),
        ));
    }
    let mut command = Command::new(git);
    command
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("LC_ALL", "C")
        .env("HOME", "/nonexistent")
        .arg("-C")
        .arg(repository)
        .args([
            "--no-replace-objects",
            "--no-lazy-fetch",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotePath=false",
            "-c",
            "core.untrackedCache=false",
        ]);
    Ok(command)
}

pub(crate) fn git_version(git: &Path) -> Result<String> {
    if !git.is_absolute() {
        return Err(Error::Invalid(
            "git executable path must be absolute".into(),
        ));
    }
    let mut command = Command::new(git);
    command.env_clear().env("LC_ALL", "C").arg("--version");
    let out = capture_with_input(
        &mut command,
        None,
        GIT_TIMEOUT,
        VERSION_OUTPUT,
        &Cancellation::default(),
    )?;
    if out.stop.is_some() || out.code != Some(0) {
        return Err(Error::UnsupportedGitVersion {
            minimum: MINIMUM_GIT_VERSION,
            found: None,
        });
    }
    let raw = std::str::from_utf8(&out.stdout).map_err(|_| Error::UnsupportedGitVersion {
        minimum: MINIMUM_GIT_VERSION,
        found: None,
    })?;
    let report = raw.strip_suffix('\n').unwrap_or(raw);
    if report.contains(['\n', '\r']) {
        return Err(Error::UnsupportedGitVersion {
            minimum: MINIMUM_GIT_VERSION,
            found: None,
        });
    }
    let version = parse_git_version(report).ok_or(Error::UnsupportedGitVersion {
        minimum: MINIMUM_GIT_VERSION,
        found: None,
    })?;
    if version < (2, 45, 0) {
        return Err(Error::UnsupportedGitVersion {
            minimum: MINIMUM_GIT_VERSION,
            found: Some(report.to_owned()),
        });
    }
    Ok(report.to_owned())
}

fn parse_git_version(report: &str) -> Option<(u64, u64, u64)> {
    fn number(value: &str) -> Option<u64> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        value.parse().ok()
    }

    let value = report.strip_prefix("git version ")?;
    let (core, suffix) = match value.split_once(' ') {
        Some((core, suffix)) => (core, Some(suffix)),
        None => (value, None),
    };
    let mut components = core.split('.');
    let major = number(components.next()?)?;
    let minor = number(components.next()?)?;
    let patch = number(components.next()?)?;
    let rest: Vec<_> = components.collect();
    if !(rest.is_empty()
        || (rest.len() == 2
            && rest[0] == "windows"
            && !rest[1].is_empty()
            && rest[1].bytes().all(|byte| byte.is_ascii_digit())))
    {
        return None;
    }
    if let Some(suffix) = suffix {
        let apple = suffix
            .strip_prefix("(Apple Git-")
            .and_then(|value| value.strip_suffix(')'))?;
        if !apple
            .split('.')
            .all(|component| number(component).is_some())
        {
            return None;
        }
    }
    Some((major, minor, patch))
}

pub(crate) struct Git<'a> {
    pub(crate) exe: &'a Path,
    pub(crate) repository: &'a Path,
    /// No command starts after this, and none runs past it.
    pub(crate) deadline: Option<Instant>,
}

impl Git<'_> {
    pub(crate) fn run(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        budget: u64,
    ) -> Result<Vec<u8>> {
        Ok(self.run_code(args, input, budget, false)?.1)
    }

    /// Returns exit code and stdout; exit 1 is accepted only when `one_ok`.
    pub(crate) fn run_code(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        budget: u64,
        one_ok: bool,
    ) -> Result<(i32, Vec<u8>)> {
        let name = args.first().copied().unwrap_or("");
        let timeout = self.deadline.map_or(GIT_TIMEOUT, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .min(GIT_TIMEOUT)
        });
        if timeout.is_zero() {
            return Err(Error::Invalid(format!(
                "git {name} not started: deadline passed"
            )));
        }
        let mut command = git_command(self.exe, self.repository)?;
        if self.deadline.is_some() {
            // Own group so a timeout also kills merge drivers and filters. Runs without a
            // deadline stay in the terminal's group, where Ctrl-C still reaches them.
            command.process_group(0);
        }
        command.args(args);
        let out = capture_with_input(
            &mut command,
            input,
            timeout,
            budget,
            &Cancellation::default(),
        )?;
        if let Some(stop) = out.stop {
            return Err(Error::Invalid(format!(
                "git {name} did not complete ({stop:?}); output may be partial"
            )));
        }
        match out.code {
            Some(0) => Ok((0, out.stdout)),
            Some(1) if one_ok => Ok((1, out.stdout)),
            code => {
                let end = out.stderr.len().min(STDERR_EXCERPT);
                Err(Error::Invalid(format!(
                    "git {name} failed with {code:?}: {}",
                    String::from_utf8_lossy(&out.stderr[..end]).trim()
                )))
            }
        }
    }

    pub(crate) fn text(&self, args: &[&str]) -> Result<String> {
        let out = self.run(args, None, SMALL_OUTPUT)?;
        Ok(String::from_utf8_lossy(&out).trim_end().to_owned())
    }
}

fn is_oid(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn object_length(format: &str) -> Result<usize> {
    match format {
        "sha1" => Ok(40),
        "sha256" => Ok(64),
        other => Err(Error::Invalid(format!("unknown object format {other}"))),
    }
}

struct RawEntry {
    mode: String,
    kind: String,
    oid: String,
    size: u64,
    path: Vec<u8>,
}

fn parse_tree(out: &[u8], oid_length: usize) -> Result<Vec<RawEntry>> {
    let bad = || Error::Invalid("malformed ls-tree output".into());
    let mut entries = Vec::new();
    for record in out.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let tab = record.iter().position(|b| *b == b'\t').ok_or_else(bad)?;
        let header = std::str::from_utf8(&record[..tab]).map_err(|_| bad())?;
        let fields: Vec<&str> = header.split_ascii_whitespace().collect();
        let [mode, kind, oid, size] = fields[..] else {
            return Err(bad());
        };
        if !is_oid(oid, oid_length) {
            return Err(bad());
        }
        let size = if size == "-" {
            0
        } else {
            size.parse::<u64>().map_err(|_| bad())?
        };
        entries.push(RawEntry {
            mode: mode.to_owned(),
            kind: kind.to_owned(),
            oid: oid.to_owned(),
            size,
            path: record[tab + 1..].to_vec(),
        });
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

fn parse_status(out: &[u8]) -> Result<(String, u64, Vec<String>, u64)> {
    let bad = |what: &str| Error::Invalid(format!("malformed status output: {what}"));
    let (mut head, mut changes, mut paths, mut untracked) = (None, 0u64, Vec::new(), 0u64);
    let mut records = out.split(|b| *b == 0);
    while let Some(record) = records.next() {
        let fields = match record.first() {
            None => continue,
            Some(b'#') => {
                if let Some(oid) = record.strip_prefix(b"# branch.oid ") {
                    head = Some(String::from_utf8_lossy(oid).into_owned());
                }
                continue;
            }
            Some(b'?') => {
                untracked += 1;
                continue;
            }
            Some(b'!') => continue,
            Some(b'1') => 9,
            Some(b'2') => {
                // A rename record is followed by its original path.
                records.next();
                10
            }
            Some(b'u') => 11,
            Some(_) => return Err(bad("unknown record")),
        };
        changes += 1;
        if paths.len() < MAX_REPORTED_PATHS {
            let path = record.splitn(fields, |b| *b == b' ').last().unwrap_or(&[]);
            paths.push(String::from_utf8_lossy(path).into_owned());
        }
    }
    let head = head.ok_or_else(|| bad("no branch.oid"))?;
    Ok((head, changes, paths, untracked))
}

fn parent_directories(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(i, _)| &path[..i])
}

/// Read and classify the committed tree. Never publishes anything.
pub fn preflight(request: &SnapshotRequest) -> Result<Preflight> {
    let git_version = git_version(&request.git)?;
    let probe = Git {
        exe: &request.git,
        repository: &request.repository,
        deadline: None,
    };
    let toplevel_bytes = probe.run(&["rev-parse", "--show-toplevel"], None, SMALL_OUTPUT)?;
    let toplevel = PathBuf::from(std::ffi::OsStr::from_bytes(
        toplevel_bytes
            .strip_suffix(b"\n")
            .unwrap_or(&toplevel_bytes),
    ));
    let git = Git {
        exe: &request.git,
        repository: &toplevel,
        deadline: None,
    };
    let git_dir_bytes = git.run(
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        None,
        SMALL_OUTPUT,
    )?;
    let git_dir = PathBuf::from(std::ffi::OsStr::from_bytes(
        git_dir_bytes.strip_suffix(b"\n").unwrap_or(&git_dir_bytes),
    ));
    let object_format = git.text(&["rev-parse", "--show-object-format"])?;
    let oid_length = object_length(&object_format)?;
    if !is_oid(&request.revision, oid_length) {
        return Err(Error::Invalid(format!(
            "revision must be a full lowercase {oid_length}-digit {object_format} commit ID"
        )));
    }
    let shallow = git.text(&["rev-parse", "--is-shallow-repository"])? == "true";
    let commit_spec = format!("{}^{{commit}}", request.revision);
    let commit = git.text(&["rev-parse", "--verify", "--end-of-options", &commit_spec])?;
    if commit != request.revision {
        return Err(Error::Invalid("revision did not resolve to itself".into()));
    }
    let tree_spec = format!("{commit}^{{tree}}");
    let tree = git.text(&["rev-parse", "--verify", "--end-of-options", &tree_spec])?;

    let (_, config) = git.run_code(
        &["config", "-z", "--get-regexp", "^filter\\."],
        None,
        SMALL_OUTPUT,
        true,
    )?;
    let mut filters: Vec<String> = config
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| {
            let key = r.split(|b| *b == b'\n').next().unwrap_or(r);
            String::from_utf8_lossy(key).into_owned()
        })
        .collect();
    filters.sort();

    // `status` and `ls-files --ignored` stat and may clean-filter tracked files, so
    // with any configured filter neither runs and cleanliness stays unverified.
    let (head, tracked_changes, tracked_change_paths, untracked_count, clean) =
        if filters.is_empty() {
            let status = git.run(
                &[
                    "status",
                    "--porcelain=v2",
                    "-z",
                    "--branch",
                    "--untracked-files=all",
                    "--ignored=no",
                    "--ignore-submodules=all",
                ],
                None,
                STATUS_OUTPUT,
            )?;
            let (head, mut changes, mut paths, untracked) = parse_status(&status)?;
            // Skip-worktree and assume-unchanged entries hide edits from `status`.
            let flags = git.run(&["ls-files", "-z", "-v"], None, TREE_OUTPUT)?;
            for record in flags.split(|b| *b == 0).filter(|r| !r.is_empty()) {
                if matches!(record[0], b'S' | b'a'..=b'z') {
                    changes += 1;
                    if paths.len() < MAX_REPORTED_PATHS {
                        let path = record.get(2..).unwrap_or(&[]);
                        paths.push(String::from_utf8_lossy(path).into_owned());
                    }
                }
            }
            let clean = head == commit && changes == 0;
            (head, changes, paths, untracked, clean)
        } else {
            let (code, out) = git.run_code(
                &["rev-parse", "--verify", "--quiet", "HEAD"],
                None,
                SMALL_OUTPUT,
                true,
            )?;
            let head = if code == 0 {
                String::from_utf8_lossy(&out).trim().to_owned()
            } else {
                "(initial)".to_owned()
            };
            (head, 0, Vec::new(), 0, false)
        };

    let tree_out = git.run(
        &["ls-tree", "-r", "-l", "-z", "--full-tree", &commit],
        None,
        TREE_OUTPUT,
    )?;
    let raw = parse_tree(&tree_out, oid_length)?;

    // Valid only when the index equals the selected commit.
    let mut ignored: Vec<String> = if clean {
        git.run(
            &[
                "ls-files",
                "-z",
                "--cached",
                "--ignored",
                "--exclude-standard",
            ],
            None,
            IGNORED_OUTPUT,
        )?
        .split(|b| *b == 0)
        .filter(|r| !r.is_empty())
        .map(|r| String::from_utf8_lossy(r).into_owned())
        .collect()
    } else {
        Vec::new()
    };
    ignored.sort();

    let mut unsupported = Vec::new();
    let mut limits = Vec::new();
    let mut entries = Vec::new();
    let mut excluded = Vec::new();
    let mut live_paths: Vec<String> = Vec::new();
    let mut used_exclusions = BTreeSet::new();
    let declared: BTreeSet<&str> = request.exclude.iter().map(String::as_str).collect();
    let mut flag = |kind, path: &str, detail: Option<String>| {
        unsupported.push(Unsupported {
            kind,
            path: path.to_owned(),
            detail,
        });
    };

    for entry in &raw {
        let path = String::from_utf8_lossy(&entry.path).into_owned();
        let utf8 = std::str::from_utf8(&entry.path).is_ok();
        let item = ManifestEntry {
            path: path.clone(),
            mode: entry.mode.clone(),
            oid: entry.oid.clone(),
            size: entry.size,
        };
        let is_symlink = entry.mode == "120000" && entry.kind == "blob";
        let mode_kind = match (entry.mode.as_str(), entry.kind.as_str()) {
            ("100644" | "100755" | "120000", "blob") => None,
            ("160000", "commit") => Some(UnsupportedKind::Gitlink),
            _ => Some(UnsupportedKind::UnknownMode),
        };
        if (is_symlink || mode_kind == Some(UnsupportedKind::Gitlink))
            && utf8
            && declared.contains(path.as_str())
        {
            used_exclusions.insert(path);
            excluded.push(item);
            continue;
        }
        if let Some(kind) = mode_kind {
            flag(kind, &path, Some(format!("mode {}", entry.mode)));
        }
        let mut path_ok = true;
        if !utf8 {
            path_ok = false;
            flag(
                UnsupportedKind::NonPortablePath,
                &path,
                Some("path is not valid UTF-8".into()),
            );
        } else if let Err(error) = validate_path(&path) {
            path_ok = false;
            flag(
                UnsupportedKind::NonPortablePath,
                &path,
                Some(error.to_string()),
            );
        }
        if uses_reserved_namespace(&path) {
            flag(UnsupportedKind::ReservedNamespace, &path, None);
        }
        let depth = path.bytes().filter(|b| *b == b'/').count();
        if depth > MAX_DEPTH {
            limits.push(LimitViolation {
                limit: LimitKind::Depth,
                observed: depth as u64,
                maximum: MAX_DEPTH as u64,
                path: Some(path.clone()),
            });
        }
        live_paths.push(path.clone());
        if mode_kind.is_none() && path_ok {
            if entry.size > MAX_FILE_BYTES as u64 {
                limits.push(LimitViolation {
                    limit: LimitKind::FileBytes,
                    observed: entry.size,
                    maximum: MAX_FILE_BYTES as u64,
                    path: Some(path),
                });
            }
            entries.push(item);
        }
    }
    // Symlink targets are the blob bytes; read and check only those blobs.
    let symlinks: Vec<ManifestEntry> = entries
        .iter()
        .filter(|entry| entry.mode == "120000")
        .cloned()
        .collect();
    if !symlinks.is_empty() {
        let readable: Vec<ManifestEntry> = symlinks
            .iter()
            .filter(|entry| entry.size <= MAX_SYMLINK_TARGET_BYTES as u64)
            .cloned()
            .collect();
        let targets = read_blobs(request, &readable)?;
        let mut bad = BTreeSet::new();
        for entry in &symlinks {
            if entry.size > MAX_SYMLINK_TARGET_BYTES as u64 {
                bad.insert(entry.path.clone());
                flag(
                    UnsupportedKind::SymlinkTarget,
                    &entry.path,
                    Some("symlink target is longer than 1024 bytes".into()),
                );
            }
        }
        for (entry, target) in readable.iter().zip(&targets) {
            if let Err(error) = validate_symlink_target(&entry.path, target) {
                bad.insert(entry.path.clone());
                flag(
                    UnsupportedKind::SymlinkTarget,
                    &entry.path,
                    Some(error.to_string()),
                );
            }
        }
        entries.retain(|entry| !bad.contains(&entry.path));
    }
    for path in &request.exclude {
        if !used_exclusions.contains(path.as_str()) {
            flag(
                UnsupportedKind::InvalidExclusion,
                path,
                Some("not a tracked symlink or gitlink".into()),
            );
        }
    }

    // Case aliases and file/directory conflicts over every path that would be captured.
    let mut spellings = PortableSpellings::default();
    let mut directories: BTreeSet<String> = BTreeSet::new();
    for path in &live_paths {
        directories.extend(parent_directories(&portable_path(path)).map(str::to_owned));
    }
    for path in &live_paths {
        let key = portable_path(path);
        if let Some(other) = spellings.insert(path) {
            flag(UnsupportedKind::CaseAlias, path, Some(other.to_owned()));
        }
        if directories.contains(&key) {
            flag(
                UnsupportedKind::FileDirectoryConflict,
                path,
                Some("path is also a directory".into()),
            );
        }
    }
    for path in &ignored {
        flag(
            UnsupportedKind::IgnoredTracked,
            path,
            Some("tracked path matches repository ignore rules".into()),
        );
    }

    let mut dirs = BTreeSet::new();
    let mut bytes = 0u64;
    for entry in &entries {
        dirs.extend(parent_directories(&entry.path));
        bytes += entry.size;
    }
    let totals = Totals {
        files: entries.len() as u64,
        directories: dirs.len() as u64,
        bytes,
    };
    if entries.len() + dirs.len() >= MAX_ENTRIES {
        limits.push(LimitViolation {
            limit: LimitKind::Entries,
            observed: (entries.len() + dirs.len()) as u64,
            maximum: MAX_ENTRIES as u64 - 1,
            path: None,
        });
    }
    if bytes > MAX_CHECKPOINT_BYTES as u64 {
        limits.push(LimitViolation {
            limit: LimitKind::TotalBytes,
            observed: bytes,
            maximum: MAX_CHECKPOINT_BYTES as u64,
            path: None,
        });
    }

    let mut merged: Vec<&ManifestEntry> = entries.iter().chain(&excluded).collect();
    merged.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    let mut hash = Sha256::new();
    for entry in merged {
        hash.update(format!(
            "{} {} {} {}\0",
            entry.mode, entry.oid, entry.size, entry.path
        ));
    }
    let manifest_sha256 = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();

    let eligible = clean && unsupported.is_empty() && limits.is_empty() && filters.is_empty();
    // All-regular captures keep the v1 profile, so their receipts are unchanged.
    let profile = if entries.iter().any(|entry| entry.mode != "100644") {
        PROFILE_V2
    } else {
        PROFILE
    };
    Ok(Preflight {
        profile,
        git_version,
        git_dir: git_dir.to_string_lossy().into_owned(),
        repository: toplevel.to_string_lossy().into_owned(),
        object_format,
        shallow,
        commit,
        tree,
        cleanliness: Cleanliness {
            head,
            clean,
            tracked_changes,
            tracked_change_paths,
            untracked_count,
        },
        filters,
        entries,
        excluded,
        unsupported,
        totals,
        limit_violations: limits,
        manifest_sha256,
        eligible,
    })
}

/// Read the exact blobs of manifest entries through `git cat-file --batch`, in order.
///
/// Every blob must be present, have the manifest size, and re-hash to its object
/// ID using the repository's SHA-1 or SHA-256 object format.
pub fn read_blobs(request: &SnapshotRequest, entries: &[ManifestEntry]) -> Result<Vec<Vec<u8>>> {
    read_blobs_with_progress(request, entries, &mut |_, _| {})
}

/// [`read_blobs`], calling `on_chunk(files_read, total_files)` after each
/// `cat-file` chunk: at most one call per chunk (never more than one per file).
pub fn read_blobs_with_progress(
    request: &SnapshotRequest,
    entries: &[ManifestEntry],
    on_chunk: &mut dyn FnMut(usize, usize),
) -> Result<Vec<Vec<u8>>> {
    let mut total = 0u64;
    for entry in entries {
        let limit = match entry.mode.as_str() {
            "100644" | "100755" => MAX_FILE_BYTES as u64,
            "120000" => MAX_SYMLINK_TARGET_BYTES as u64,
            _ => 0,
        };
        if !matches!(entry.mode.as_str(), "100644" | "100755" | "120000")
            || entry.size > limit
            || !(is_oid(&entry.oid, 40) || is_oid(&entry.oid, 64))
        {
            return Err(Error::Invalid(format!(
                "manifest entry {} is not a readable blob of a supported mode",
                entry.path
            )));
        }
        total += entry.size;
    }
    if total > MAX_CHECKPOINT_BYTES as u64 {
        return Err(Error::Limit(
            "manifest exceeds the checkpoint byte bound".into(),
        ));
    }
    let git = Git {
        exe: &request.git,
        repository: &request.repository,
        deadline: None,
    };
    let mut blobs = Vec::with_capacity(entries.len());
    let mut start = 0;
    while start < entries.len() {
        let mut end = start;
        let mut bytes = 0u64;
        while end < entries.len()
            && end - start < BLOB_CHUNK_FILES
            && (end == start || bytes + entries[end].size <= BLOB_CHUNK_BYTES)
        {
            bytes += entries[end].size;
            end += 1;
        }
        let chunk = &entries[start..end];
        let input: String = chunk.iter().map(|e| format!("{}\n", e.oid)).collect();
        let budget = bytes + chunk.len() as u64 * BLOB_HEADER_ALLOWANCE + 4096;
        let out = git.run(&["cat-file", "--batch"], Some(input.into_bytes()), budget)?;
        let mut rest = &out[..];
        for entry in chunk {
            blobs.push(take_blob(&mut rest, entry)?);
        }
        if !rest.is_empty() {
            return Err(Error::Invalid(
                "cat-file returned unexpected extra output".into(),
            ));
        }
        start = end;
        on_chunk(start, entries.len());
    }
    Ok(blobs)
}

fn take_blob(rest: &mut &[u8], entry: &ManifestEntry) -> Result<Vec<u8>> {
    let bad = || Error::Invalid(format!("malformed cat-file output for {}", entry.path));
    let newline = rest.iter().position(|b| *b == b'\n').ok_or_else(bad)?;
    let header = std::str::from_utf8(&rest[..newline]).map_err(|_| bad())?;
    let fields: Vec<&str> = header.split(' ').collect();
    if fields.len() == 2 && fields[0] == entry.oid && fields[1] == "missing" {
        return Err(Error::MissingObject(entry.oid.clone()));
    }
    let [oid, kind, size] = fields[..] else {
        return Err(bad());
    };
    if oid != entry.oid || kind != "blob" || size.parse::<u64>().ok() != Some(entry.size) {
        return Err(Error::Invalid(format!(
            "object for {} does not match its manifest entry",
            entry.path
        )));
    }
    let start = newline + 1;
    let end = start + entry.size as usize;
    if rest.len() < end + 1 || rest[end] != b'\n' {
        return Err(bad());
    }
    let content = rest[start..end].to_vec();
    *rest = &rest[end + 1..];
    let header = format!("blob {}\0", content.len());
    let actual: String = if entry.oid.len() == 40 {
        let mut hash = Context::new(&SHA1_FOR_LEGACY_USE_ONLY);
        hash.update(header.as_bytes());
        hash.update(&content);
        hash.finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    } else {
        let mut hash = Sha256::new();
        hash.update(header.as_bytes());
        hash.update(&content);
        hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
    };
    if actual != entry.oid {
        return Err(Error::Invalid(format!(
            "content of {} does not hash to its object ID",
            entry.path
        )));
    }
    Ok(content)
}

pub const RECEIPT: &str = "agentscm.git-snapshot-receipt/v1";
const PROVENANCE: &str =
    "local operator declaration; Git observations are unauthenticated and are not review evidence";
const MAX_RECEIPT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct NativeRecord {
    /// Authority repository ID the revision is intended for.
    pub repository: String,
    pub revision: String,
    pub batches: usize,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct GitRecord {
    pub version: String,
    pub repository: String,
    pub object_format: String,
    pub shallow: bool,
    pub commit: String,
    pub tree: String,
}

/// Deterministic receipt: no timestamps, process IDs or randomness.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Receipt {
    pub receipt: &'static str,
    pub profile: &'static str,
    pub provenance: &'static str,
    pub native: NativeRecord,
    pub git: GitRecord,
    pub cleanliness: Cleanliness,
    pub manifest_sha256: String,
    pub entries: Vec<ManifestEntry>,
    pub excluded: Vec<ManifestEntry>,
    pub totals: Totals,
    /// `sha1-rehashed` or `sha256-rehashed`, according to the Git object format.
    pub content_verification: &'static str,
}

impl Receipt {
    /// Canonical receipt bytes: compact JSON and a trailing newline.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut bytes = serde_json::to_vec(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}

/// Split sorted entries into contiguous batches that each obey the given limits
/// (changes, summed `/` count of paths, content bytes). An entry that alone
/// exceeds a limit still gets its own batch, and the store then rejects it.
pub fn plan_batches(
    entries: &[ManifestEntry],
    max_changes: usize,
    max_prefixes: usize,
    max_bytes: u64,
) -> Vec<Range<usize>> {
    let mut batches = Vec::new();
    let (mut start, mut prefixes, mut bytes) = (0, 0usize, 0u64);
    for (index, entry) in entries.iter().enumerate() {
        let entry_prefixes = entry.path.bytes().filter(|b| *b == b'/').count();
        if index > start
            && (index - start >= max_changes
                || prefixes + entry_prefixes > max_prefixes
                || bytes + entry.size > max_bytes)
        {
            batches.push(start..index);
            (start, prefixes, bytes) = (index, 0, 0);
        }
        prefixes += entry_prefixes;
        bytes += entry.size;
    }
    if start < entries.len() {
        batches.push(start..entries.len());
    }
    batches
}

fn ineligible_message(report: &Preflight) -> String {
    let mut reasons = Vec::new();
    if !report.filters.is_empty() {
        reasons.push(format!(
            "checkout cleanliness not verified because filters are configured: {}",
            report.filters.join(", ")
        ));
    } else if !report.cleanliness.clean {
        reasons.push(format!(
            "checkout is not clean: HEAD {} vs commit {}, {} tracked change(s) [{}]",
            report.cleanliness.head,
            report.commit,
            report.cleanliness.tracked_changes,
            report.cleanliness.tracked_change_paths.join(", ")
        ));
    }
    for item in report.unsupported.iter().take(MAX_REPORTED_PATHS) {
        reasons.push(format!(
            "unsupported {:?} at {}{}",
            item.kind,
            item.path,
            item.detail
                .as_ref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        ));
    }
    if report.unsupported.len() > MAX_REPORTED_PATHS {
        reasons.push(format!(
            "{} more unsupported entries",
            report.unsupported.len() - MAX_REPORTED_PATHS
        ));
    }
    for limit in report.limit_violations.iter().take(MAX_REPORTED_PATHS) {
        reasons.push(format!(
            "limit {:?} exceeded: {} > {}{}",
            limit.limit,
            limit.observed,
            limit.maximum,
            limit
                .path
                .as_ref()
                .map(|p| format!(" at {p}"))
                .unwrap_or_default()
        ));
    }
    format!("snapshot is not eligible: {}", reasons.join("; "))
}

/// Published snapshot and the receipt describing it.
pub struct Snapshot {
    pub revision: String,
    pub batches: usize,
    pub receipt: Receipt,
}

/// Capture the committed tree as one native root revision (no parent).
///
/// Restart and idempotency: objects are content-addressed and the revision
/// contains no timestamp, so rerunning the same request against the same
/// committed tree publishes the identical revision. An interruption before or
/// during publication leaves only unreferenced objects, which are retained
/// (there is no garbage collection) and reused by a rerun. No target, Change or
/// approval is touched. The receipt is written last, by [`write_receipt`]; an
/// ineligible tree fails before any object is written. `report` must be the
/// [`preflight`] of `request`; the caller runs it once and may inspect it first.
pub fn capture(
    engine: &Engine,
    repository_id: &str,
    request: &SnapshotRequest,
    report: Preflight,
) -> Result<Snapshot> {
    capture_with_progress(engine, repository_id, request, report, &mut |_| {})
}

/// [`capture`], reporting progress lines (no trailing newline) to `progress`.
///
/// The line count is bounded by construction: one per blob-read chunk, one per
/// store batch, and one final "published" line, so at most
/// `chunks + batches + 1` lines, where chunks never exceed the file count.
pub fn capture_with_progress(
    engine: &Engine,
    repository_id: &str,
    request: &SnapshotRequest,
    report: Preflight,
    progress: &mut dyn FnMut(&str),
) -> Result<Snapshot> {
    if !report.eligible {
        return Err(Error::Conflict(ineligible_message(&report)));
    }
    let toplevel = SnapshotRequest {
        repository: PathBuf::from(&report.repository),
        ..request.clone()
    };
    let blobs = read_blobs_with_progress(&toplevel, &report.entries, &mut |done, total| {
        progress(&format!("read {done}/{total} files"));
    })?;
    let changes: Vec<EntryChange> = report
        .entries
        .iter()
        .zip(blobs)
        .map(|(entry, content)| EntryChange {
            path: entry.path.clone(),
            content: Some(content),
            kind: match entry.mode.as_str() {
                "100755" => EntryKind::Executable,
                "120000" => EntryKind::Symlink,
                _ => EntryKind::Regular,
            },
        })
        .collect();
    let plan = plan_store_batches(&report.entries);
    let revision = engine.checkpoint_entry_batches_with_progress(
        None,
        plan.iter().map(|range| &changes[range.clone()]),
        &mut |done, total| progress(&format!("wrote batch {done}/{total}")),
    )?;
    let batches = plan.len();
    progress(&format!("published revision {revision}"));
    Ok(Snapshot {
        revision: revision.clone(),
        batches,
        receipt: build_receipt(report, repository_id, revision, batches),
    })
}

fn plan_store_batches(entries: &[ManifestEntry]) -> Vec<Range<usize>> {
    plan_batches(
        entries,
        MAX_CHANGES,
        MAX_CHECKPOINT_PREFIXES,
        MAX_CHECKPOINT_BYTES as u64,
    )
}

fn build_receipt(
    report: Preflight,
    repository_id: &str,
    revision: String,
    batches: usize,
) -> Receipt {
    let content_verification = if report.object_format == "sha256" {
        "sha256-rehashed"
    } else {
        "sha1-rehashed"
    };
    Receipt {
        receipt: RECEIPT,
        profile: report.profile,
        provenance: PROVENANCE,
        native: NativeRecord {
            repository: repository_id.to_owned(),
            revision,
            batches,
        },
        git: GitRecord {
            version: report.git_version,
            repository: report.repository,
            object_format: report.object_format,
            shallow: report.shallow,
            commit: report.commit,
            tree: report.tree,
        },
        cleanliness: report.cleanliness,
        manifest_sha256: report.manifest_sha256,
        entries: report.entries,
        excluded: report.excluded,
        totals: report.totals,
        content_verification,
    }
}

/// Fail with a conflict, before any publication, when `path` holds anything
/// other than the receipt this request would write. Every field is compared
/// except `native.revision`, which is only known after publication.
pub fn check_receipt_path(path: &Path, report: &Preflight, repository_id: &str) -> Result<()> {
    let conflict = |why: &str| Error::Conflict(format!("receipt path {} {why}", path.display()));
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(Error::Io(error)),
    };
    if !metadata.is_file() || metadata.len() > MAX_RECEIPT_BYTES {
        return Err(conflict("exists and is not a bounded regular file"));
    }
    let bytes = crate::sync::read_bounded(path, MAX_RECEIPT_BYTES, "receipt")?;
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| conflict("exists and is not a receipt"))?;
    if !value.is_object() {
        return Err(conflict("exists and is not a receipt"));
    }
    if !report.eligible {
        // Capture refuses an ineligible tree with a precise message.
        return Ok(());
    }
    let plan = plan_store_batches(&report.entries);
    let mut expected = serde_json::to_value(build_receipt(
        report.clone(),
        repository_id,
        String::new(),
        plan.len(),
    ))?;
    let mut actual = value;
    for receipt in [&mut expected, &mut actual] {
        if let Some(native) = receipt
            .get_mut("native")
            .and_then(serde_json::Value::as_object_mut)
        {
            native.remove("revision");
        }
    }
    if actual != expected {
        return Err(conflict(
            "exists with different content than this request would write",
        ));
    }
    Ok(())
}

/// Write the receipt to a new file, never overwriting. Returns `true` when the
/// path already held byte-identical content (idempotent rerun); different
/// existing content is a conflict and the file is left untouched.
pub fn write_receipt(path: &Path, receipt: &Receipt) -> Result<bool> {
    let bytes = receipt.to_bytes()?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.is_file()
                && metadata.len() <= MAX_RECEIPT_BYTES
                && crate::sync::read_bounded(path, MAX_RECEIPT_BYTES, "receipt")? == bytes
            {
                return Ok(true);
            }
            Err(Error::Conflict(format!(
                "receipt path exists with different content: {}",
                path.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::sync::write_new(path, |file| file.write_all(&bytes))?;
            Ok(false)
        }
        Err(error) => Err(Error::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_version_parser_accepts_documented_reports_only() {
        for report in [
            "git version 2.45.0",
            "git version 2.50.1 (Apple Git-155)",
            "git version 2.50.1 (Apple Git-155.1)",
            "git version 2.45.0.windows.1",
        ] {
            assert!(parse_git_version(report).is_some(), "{report}");
        }
        for report in [
            "git version +2.45.0",
            "git version 2.+45.0",
            "git version 2.45.+0",
            "git version 2.45.0.rc1",
            "git version 2.45",
            "git version 2.45.0.1",
            "git version 2.45.0.windows.",
            "git version 2.45.0 (Apple Git-155..1)",
            "git version 18446744073709551616.45.0",
        ] {
            assert!(parse_git_version(report).is_none(), "{report}");
        }
    }

    fn blob_entry(path: &str, oid: &str, size: u64) -> ManifestEntry {
        ManifestEntry {
            path: path.into(),
            mode: "100644".into(),
            oid: oid.into(),
            size,
        }
    }

    #[test]
    fn take_blob_rehashes_known_empty_blob_object_ids() {
        for oid in [
            "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391",
            "473a0f4c3be8a93681a267e3b1e9a7dcda1185436fe141f7749120a303721813",
        ] {
            let output = format!("{oid} blob 0\n\n");
            let mut rest = output.as_bytes();
            assert_eq!(
                take_blob(&mut rest, &blob_entry("empty", oid, 0)).unwrap(),
                b""
            );
            assert!(rest.is_empty());
        }
    }

    #[test]
    fn take_blob_rejects_content_that_does_not_match_object_id() {
        let oid = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
        let output = format!("{oid} blob 1\n\0\n");
        let mut rest = output.as_bytes();
        let error = take_blob(&mut rest, &blob_entry("binary.dat", oid, 1))
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "invalid: content of binary.dat does not hash to its object ID"
        );
    }

    #[test]
    fn timeout_kills_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = dir.path().join("git");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\n/bin/sleep 30 &\necho $! > {}\nwait\n",
                pid_file.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let git = Git {
            exe: &script,
            repository: dir.path(),
            deadline: Some(Instant::now() + Duration::from_secs(5)),
        };
        assert!(git.run(&["status"], None, 1024).is_err());

        let pid: i32 = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        // A killed orphan may linger briefly as a zombie until init reaps it.
        let end = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(pid, 0) } != -1 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "grandchild survived");
    }
}
