//! Bounded, read-only Git checkout exploration.
//!
//! The checkout and executable are selected once at startup. Requests can name
//! immutable commit objects and in-tree paths, but never filesystem paths. Git
//! runs with network-backed lazy fetching, hooks, pagers, external diffs,
//! textconv and optional locks disabled. Working-tree observation never asks
//! Git to inspect tracked file content, so repository clean/process filters
//! are not executed.

use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::execution::Cancellation;
use crate::execution::process::{Stop, capture_with_input};
use crate::{Error, Result};

const VERSION: u32 = 1;
const REQUEST_DEADLINE: Duration = Duration::from_secs(30);
const MAX_CURSOR_BYTES: usize = 2 * 1024;
const MAX_PATH_BYTES: usize = 4 * 1024;
const MAX_QUERY_BYTES: usize = 1024;
const MAX_REF_BYTES: usize = 1024;
const MAX_REPOSITORY_ID_BYTES: usize = 256;
const MAX_REFS: usize = 100;
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_JSON_CONTENT_BYTES: usize = 2 * 1024 * 1024;
const MAX_SEARCH_TEXT_BYTES: usize = 16 * 1024;
const MAX_HISTORY_NAME_BYTES: usize = 1024;
const MAX_HISTORY_SUBJECT_BYTES: usize = 8 * 1024;
const MAX_COMMIT_PARENTS: usize = 256;
const SMALL_OUTPUT: u64 = 64 * 1024;
const STATUS_OUTPUT: u64 = 1024 * 1024;
const REFS_OUTPUT: u64 = 2 * 1024 * 1024;
const TREE_OUTPUT: u64 = 16 * 1024 * 1024;
const HISTORY_OUTPUT: u64 = 4 * 1024 * 1024;
const DIFF_OUTPUT: u64 = 16 * 1024 * 1024;
const SEARCH_OUTPUT: u64 = 16 * 1024 * 1024;
const MAX_CURSOR_OFFSET: usize = 1_000_000;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitSummaryV1 {
    pub version: u32,
    pub repository_id: String,
    pub repository_label: String,
    pub selected_ref: String,
    pub commit: String,
    pub tree: String,
    pub observed_head: Option<String>,
    pub head_ref: Option<String>,
    pub dirty: Option<bool>,
    pub observed_at: i64,
    pub refs: Vec<GitRefV1>,
    pub refs_omitted: bool,
    pub shallow: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitRefV1 {
    pub name: String,
    pub commit: String,
    pub kind: GitRefKindV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitRefKindV1 {
    LocalBranch,
    Tag,
    RemoteTracking,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitTreePageV1 {
    pub version: u32,
    pub repository_id: String,
    pub commit: String,
    pub tree: String,
    pub path: String,
    pub entries: Vec<GitTreeEntryV1>,
    pub next: Option<String>,
    pub complete: bool,
    pub limit_state: PageLimitStateV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitTreeEntryV1 {
    pub path: String,
    pub name: String,
    pub kind: GitEntryKindV1,
    pub oid: String,
    pub size: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitEntryKindV1 {
    Directory,
    File,
    Executable,
    Symlink,
    Gitlink,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitFileV1 {
    pub version: u32,
    pub repository_id: String,
    pub commit: String,
    pub tree: String,
    pub path: String,
    pub kind: GitEntryKindV1,
    pub oid: String,
    pub size: Option<u64>,
    pub state: GitFileStateV1,
    pub content: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitFileStateV1 {
    Text,
    Binary,
    TooLarge,
    Unavailable,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitHistoryPageV1 {
    pub version: u32,
    pub repository_id: String,
    pub commit: String,
    pub tree: String,
    pub path: Option<String>,
    pub path_interpretation: GitPathHistoryV1,
    pub entries: Vec<GitHistoryEntryV1>,
    pub next: Option<String>,
    pub complete: bool,
    pub limit_state: PageLimitStateV1,
    pub shallow: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitPathHistoryV1 {
    Exact,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitHistoryEntryV1 {
    pub oid: String,
    pub parents: Vec<String>,
    pub author_name: String,
    pub author_time: i64,
    pub subject: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitDiffPageV1 {
    pub version: u32,
    pub repository_id: String,
    pub left: String,
    pub left_tree: String,
    pub right: String,
    pub right_tree: String,
    pub path: Option<String>,
    pub changes: Vec<GitDiffChangeV1>,
    pub patch: GitPatchV1,
    pub next: Option<String>,
    pub complete: bool,
    pub limit_state: PageLimitStateV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitDiffChangeV1 {
    pub old_path: Option<String>,
    pub path: Option<String>,
    pub kind: GitDiffKindV1,
    pub similarity: Option<u8>,
    pub old_oid: Option<String>,
    pub oid: Option<String>,
    pub old_mode: Option<String>,
    pub mode: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitDiffKindV1 {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    TypeChanged,
    Unmerged,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitPatchV1 {
    pub state: GitPatchStateV1,
    pub content: Option<String>,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GitPatchStateV1 {
    NotRequested,
    Text,
    Binary,
    TooLarge,
    Unavailable,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitSearchPageV1 {
    pub version: u32,
    pub repository_id: String,
    pub commit: String,
    pub tree: String,
    pub query: String,
    pub matches: Vec<GitSearchMatchV1>,
    pub next: Option<String>,
    pub complete: bool,
    pub limit_state: PageLimitStateV1,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GitSearchMatchV1 {
    pub path: String,
    pub line: u64,
    pub text: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PageLimitStateV1 {
    Complete,
    MoreResults,
    OutputLimited,
}

#[derive(Clone, Debug)]
pub struct GitExplorer {
    root: PathBuf,
    root_pin: FilePin,
    git_metadata_path: PathBuf,
    git_metadata_pin: FilePin,
    git_metadata_contents: Option<Vec<u8>>,
    git_dir: Option<(PathBuf, FilePin)>,
    git_common_dir: Option<(PathBuf, FilePin)>,
    git: PathBuf,
    git_pin: FilePin,
    repository_id: String,
    repository_label: String,
    object_format: ObjectFormat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ObjectFormat {
    Sha1,
    Sha256,
}

impl ObjectFormat {
    fn oid_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FilePin {
    device: u64,
    inode: u64,
    kind: fs::FileType,
}

#[derive(Debug, PartialEq, Eq)]
struct GitOutput {
    code: Option<i32>,
    stdout: Vec<u8>,
    output_limited: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct WorktreeObservation {
    dirty: Option<bool>,
    staged: GitOutput,
    untracked: GitOutput,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PageCursor {
    version: u32,
    repository_id: String,
    operation: String,
    commit: Option<String>,
    left: Option<String>,
    right: Option<String>,
    path_digest: Option<String>,
    query_digest: Option<String>,
    limit: usize,
    offset: usize,
}

#[derive(Clone, Debug)]
struct RawTreeEntry {
    path: String,
    kind: GitEntryKindV1,
    oid: String,
    size: Option<u64>,
}

impl FilePin {
    fn read(path: &Path) -> Result<Self> {
        let metadata = fs::symlink_metadata(path)
            .map_err(|_| Error::Invalid("configured repository is unavailable".into()))?;
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            kind: metadata.file_type(),
        })
    }

    fn matches(&self, path: &Path) -> bool {
        fs::symlink_metadata(path).is_ok_and(|metadata| {
            metadata.dev() == self.device
                && metadata.ino() == self.inode
                && metadata.file_type() == self.kind
        })
    }
}

impl GitExplorer {
    /// Canonical operator-selected checkout; never chosen by an HTTP request.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn repository_id(&self) -> &str {
        &self.repository_id
    }

    pub fn open(root: &Path, repository_id: &str) -> Result<Self> {
        validate_repository_id(repository_id)?;
        if !root.is_absolute() {
            return Err(Error::Invalid(
                "explorer repository path must be absolute".into(),
            ));
        }
        let root = fs::canonicalize(root)
            .map_err(|_| Error::Invalid("configured repository is unavailable".into()))?;
        let root_pin = FilePin::read(&root)?;
        if !root_pin.kind.is_dir() {
            return Err(Error::Invalid(
                "configured repository is not a directory".into(),
            ));
        }
        let git_metadata_path = root.join(".git");
        let git_metadata_pin = FilePin::read(&git_metadata_path)?;
        if git_metadata_pin.kind.is_symlink()
            || !(git_metadata_pin.kind.is_dir() || git_metadata_pin.kind.is_file())
        {
            return Err(Error::Invalid(
                "configured repository metadata has an unsupported type".into(),
            ));
        }
        let git_metadata_contents = if git_metadata_pin.kind.is_file() {
            let contents = fs::read(&git_metadata_path)
                .map_err(|_| Error::Invalid("configured repository is unavailable".into()))?;
            if contents.len() > MAX_PATH_BYTES {
                return Err(Error::Invalid(
                    "configured repository metadata is invalid".into(),
                ));
            }
            Some(contents)
        } else {
            None
        };
        let git = find_git()?;
        let git_pin = FilePin::read(&git)?;
        if !git_pin.kind.is_file() {
            return Err(Error::Invalid("Git executable is unavailable".into()));
        }
        let repository_label = root
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(repository_id)
            .chars()
            .take(256)
            .collect::<String>();
        let mut explorer = Self {
            root,
            root_pin,
            git_metadata_path,
            git_metadata_pin,
            git_metadata_contents,
            git_dir: None,
            git_common_dir: None,
            git,
            git_pin,
            repository_id: repository_id.to_owned(),
            repository_label,
            object_format: ObjectFormat::Sha1,
        };
        let deadline = deadline();
        let inside = explorer.run_complete(
            &["rev-parse", "--is-inside-work-tree"],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0],
        )?;
        if trim_line(&inside.stdout)? != "true" {
            return Err(Error::Invalid(
                "configured path is not a Git worktree".into(),
            ));
        }
        let top = explorer.run_complete(
            &["rev-parse", "--show-toplevel"],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0],
        )?;
        let top = path_from_line(&top.stdout)?;
        let top = fs::canonicalize(top)
            .map_err(|_| Error::Invalid("configured repository is unavailable".into()))?;
        if top != explorer.root {
            return Err(Error::Invalid(
                "explorer repository path must be the worktree root".into(),
            ));
        }
        let git_dir = explorer.run_complete(
            &["rev-parse", "--absolute-git-dir"],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0],
        )?;
        let git_dir = canonical_git_directory(&git_dir.stdout, &explorer.root)?;
        let git_dir_pin = FilePin::read(&git_dir)?;
        if !git_dir_pin.kind.is_dir() {
            return Err(Error::Invalid(
                "configured Git metadata directory is invalid".into(),
            ));
        }
        explorer.git_dir = Some((git_dir, git_dir_pin));
        let common_dir = explorer.run_complete(
            &["rev-parse", "--git-common-dir"],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0],
        )?;
        let common_dir = canonical_git_directory(&common_dir.stdout, &explorer.root)?;
        let common_dir_pin = FilePin::read(&common_dir)?;
        if !common_dir_pin.kind.is_dir() {
            return Err(Error::Invalid(
                "configured Git common directory is invalid".into(),
            ));
        }
        explorer.git_common_dir = Some((common_dir, common_dir_pin));
        let format = explorer.run_complete(
            &["rev-parse", "--show-object-format"],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0],
        )?;
        explorer.object_format = match trim_line(&format.stdout)? {
            "sha1" => ObjectFormat::Sha1,
            "sha256" => ObjectFormat::Sha256,
            _ => {
                return Err(Error::Invalid(
                    "repository uses an unsupported object format".into(),
                ));
            }
        };
        Ok(explorer)
    }

    pub fn validate_commit(&self, commit: &str) -> Result<()> {
        let deadline = deadline();
        self.commit_tree(commit, deadline).map(|_| ())
    }

    pub fn summary(&self, selected_ref: Option<&str>) -> Result<GitSummaryV1> {
        let deadline = deadline();
        let selected_ref = selected_ref.unwrap_or("HEAD");
        validate_ref(selected_ref)?;
        let commit = self.resolve_ref(selected_ref, deadline)?;
        let tree = self.commit_tree(&commit, deadline)?;
        let observed_head = self.observed_head(deadline)?;
        if selected_ref == "HEAD" && observed_head.as_deref() != Some(commit.as_str()) {
            return Err(Error::Conflict(
                "repository HEAD changed during summary".into(),
            ));
        }
        let head_ref = self.head_ref(deadline)?;
        self.verify_symbolic_head(head_ref.as_deref(), observed_head.as_deref(), deadline)?;
        let worktree = self.worktree_observation(observed_head.as_deref(), deadline)?;
        let (refs, refs_omitted) = self.refs(deadline)?;
        let shallow = self.is_shallow(deadline)?;

        let confirm_commit = self.resolve_ref(selected_ref, deadline)?;
        let confirm_head = self.observed_head(deadline)?;
        let confirm_head_ref = self.head_ref(deadline)?;
        self.verify_symbolic_head(
            confirm_head_ref.as_deref(),
            confirm_head.as_deref(),
            deadline,
        )?;
        let confirm_worktree = self.worktree_observation(confirm_head.as_deref(), deadline)?;
        let (confirm_refs, confirm_refs_omitted) = self.refs(deadline)?;
        let confirm_shallow = self.is_shallow(deadline)?;
        if confirm_commit != commit
            || confirm_head != observed_head
            || confirm_head_ref != head_ref
            || confirm_worktree != worktree
            || confirm_refs != refs
            // Equal repeated samples can still observe HEAD=A and its listed ref=B.
            // Check the fields returned together, while preserving partial ref pages.
            || refs.iter().any(|reference| {
                head_ref.as_deref() == Some(reference.name.as_str())
                    && observed_head.as_deref() != Some(reference.commit.as_str())
            })
            || confirm_refs_omitted != refs_omitted
            || confirm_shallow != shallow
            || (selected_ref == "HEAD" && confirm_head.as_deref() != Some(confirm_commit.as_str()))
        {
            return Err(Error::Conflict(
                "repository state changed during summary".into(),
            ));
        }
        Ok(GitSummaryV1 {
            version: VERSION,
            repository_id: self.repository_id.clone(),
            repository_label: self.repository_label.clone(),
            selected_ref: selected_ref.to_owned(),
            commit,
            tree,
            observed_head,
            head_ref,
            dirty: worktree.dirty,
            observed_at: unix_seconds()?,
            refs,
            refs_omitted,
            shallow,
        })
    }

    pub fn tree(
        &self,
        commit: &str,
        path: Option<&str>,
        after: Option<&str>,
        limit: usize,
    ) -> Result<GitTreePageV1> {
        validate_limit(limit, 200, "tree")?;
        let path = normalize_optional_path(path)?;
        let deadline = deadline();
        let tree = self.commit_tree(commit, deadline)?;
        let cursor = self.page_cursor(
            after,
            "tree",
            Some(commit),
            None,
            None,
            path.as_deref(),
            None,
            limit,
        )?;
        let selected_tree = match path.as_deref() {
            None | Some("") => tree.clone(),
            Some(path) => {
                let entry = self
                    .tree_entry(commit, path, deadline)?
                    .ok_or_else(|| Error::NotFound("repository path".into()))?;
                if entry.kind != GitEntryKindV1::Directory {
                    return Err(Error::Invalid("repository path is not a directory".into()));
                }
                entry.oid
            }
        };
        let out = self.run_complete(
            &["ls-tree", "-z", "-l", "--full-name", &selected_tree],
            None,
            TREE_OUTPUT,
            deadline,
            &[0],
        )?;
        let raw = parse_tree(&out.stdout, self.object_format)?;
        let mut all = Vec::with_capacity(raw.len());
        let prefix = path.as_deref().unwrap_or("");
        for entry in raw {
            let name = entry.path;
            let full_path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            all.push(GitTreeEntryV1 {
                path: full_path,
                name,
                kind: entry.kind,
                oid: entry.oid,
                size: entry.size,
            });
        }
        let (entries, next, complete, limit_state) =
            self.page(all, cursor.offset, limit, out.output_limited, cursor)?;
        Ok(GitTreePageV1 {
            version: VERSION,
            repository_id: self.repository_id.clone(),
            commit: commit.to_owned(),
            tree,
            path: path.unwrap_or_default(),
            entries,
            next,
            complete,
            limit_state,
        })
    }

    pub fn file(&self, commit: &str, path: &str) -> Result<GitFileV1> {
        let path = validate_path(path, false)?.to_owned();
        let deadline = deadline();
        let tree = self.commit_tree(commit, deadline)?;
        let entry = self
            .tree_entry(commit, &path, deadline)?
            .ok_or_else(|| Error::NotFound("repository path".into()))?;
        let (state, content) = match entry.kind {
            GitEntryKindV1::File | GitEntryKindV1::Executable => {
                if entry.size.is_none_or(|size| size > MAX_FILE_BYTES) {
                    (GitFileStateV1::TooLarge, None)
                } else {
                    match self.blob(&entry.oid, deadline)? {
                        None => (GitFileStateV1::Unavailable, None),
                        Some(bytes) if bytes.contains(&0) => (GitFileStateV1::Binary, None),
                        Some(bytes) => match String::from_utf8(bytes) {
                            Ok(content) if json_string_size(&content) <= MAX_JSON_CONTENT_BYTES => {
                                (GitFileStateV1::Text, Some(content))
                            }
                            Ok(_) => (GitFileStateV1::TooLarge, None),
                            Err(_) => (GitFileStateV1::Binary, None),
                        },
                    }
                }
            }
            GitEntryKindV1::Directory | GitEntryKindV1::Symlink | GitEntryKindV1::Gitlink => {
                (GitFileStateV1::Unsupported, None)
            }
        };
        Ok(GitFileV1 {
            version: VERSION,
            repository_id: self.repository_id.clone(),
            commit: commit.to_owned(),
            tree,
            path,
            kind: entry.kind,
            oid: entry.oid,
            size: entry.size,
            state,
            content,
        })
    }

    pub fn history(
        &self,
        commit: &str,
        path: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<GitHistoryPageV1> {
        validate_limit(limit, 100, "history")?;
        let path = normalize_optional_path(path)?;
        let deadline = deadline();
        let tree = self.commit_tree(commit, deadline)?;
        let page_cursor = self.page_cursor(
            cursor,
            "history",
            Some(commit),
            None,
            None,
            path.as_deref(),
            None,
            limit,
        )?;
        let count = limit
            .checked_add(1)
            .ok_or_else(|| Error::Limit("history page limit is invalid".into()))?;
        let count_arg = format!("--max-count={count}");
        let skip_arg = format!("--skip={}", page_cursor.offset);
        let mut args = vec![
            "log",
            "--no-decorate",
            "--no-color",
            "--format=format:%H%x00%P%x00%an%x00%at%x00%s%x00%x00",
            count_arg.as_str(),
            skip_arg.as_str(),
            "--end-of-options",
            commit,
        ];
        if let Some(path) = path.as_deref() {
            args.push("--");
            args.push(path);
        }
        let out = self.run_complete(&args, None, HISTORY_OUTPUT, deadline, &[0])?;
        let (mut entries, content_limited) = parse_history(&out.stdout, self.object_format)?;
        let has_more = entries.len() > limit;
        entries.truncate(limit);
        let (next, complete, limit_state) = if out.output_limited || content_limited {
            (None, false, PageLimitStateV1::OutputLimited)
        } else if has_more {
            let mut next_cursor = page_cursor;
            next_cursor.offset = next_cursor
                .offset
                .checked_add(entries.len())
                .ok_or_else(|| Error::Limit("history cursor offset is invalid".into()))?;
            (
                Some(encode_cursor(&next_cursor)?),
                false,
                PageLimitStateV1::MoreResults,
            )
        } else {
            (None, true, PageLimitStateV1::Complete)
        };
        Ok(GitHistoryPageV1 {
            version: VERSION,
            repository_id: self.repository_id.clone(),
            commit: commit.to_owned(),
            tree,
            path,
            path_interpretation: GitPathHistoryV1::Exact,
            entries,
            next,
            complete,
            limit_state,
            shallow: self.is_shallow(deadline)?,
        })
    }

    pub fn diff(
        &self,
        left: &str,
        right: &str,
        path: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<GitDiffPageV1> {
        validate_limit(limit, 200, "diff")?;
        let path = normalize_optional_path(path)?;
        let deadline = deadline();
        let left_tree = self.commit_tree(left, deadline)?;
        let right_tree = self.commit_tree(right, deadline)?;
        let page_cursor = self.page_cursor(
            cursor,
            "diff",
            None,
            Some(left),
            Some(right),
            path.as_deref(),
            None,
            limit,
        )?;
        let mut args = vec![
            "diff-tree",
            "--no-commit-id",
            "-r",
            "-z",
            "--raw",
            "--no-abbrev",
            "--find-renames",
            "--find-copies",
            "--find-copies-harder",
            "--no-ext-diff",
            "--no-textconv",
            "--end-of-options",
            left,
            right,
        ];
        if let Some(path) = path.as_deref() {
            args.push("--");
            args.push(path);
        }
        let out = self.run_complete(&args, None, DIFF_OUTPUT, deadline, &[0])?;
        let all = parse_diff(&out.stdout, self.object_format, out.output_limited)?;
        let (changes, next, complete, limit_state) = self.page(
            all,
            page_cursor.offset,
            limit,
            out.output_limited,
            page_cursor,
        )?;
        let patch = match path.as_deref() {
            None => GitPatchV1 {
                state: GitPatchStateV1::NotRequested,
                content: None,
                truncated: false,
            },
            Some(path) => self.patch(left, right, path, deadline)?,
        };
        Ok(GitDiffPageV1 {
            version: VERSION,
            repository_id: self.repository_id.clone(),
            left: left.to_owned(),
            left_tree,
            right: right.to_owned(),
            right_tree,
            path,
            changes,
            patch,
            next,
            complete,
            limit_state,
        })
    }

    pub fn search(
        &self,
        commit: &str,
        query: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<GitSearchPageV1> {
        validate_limit(limit, 100, "search")?;
        validate_query(query)?;
        let deadline = deadline();
        let tree = self.commit_tree(commit, deadline)?;
        let query_digest = digest(query.as_bytes());
        let page_cursor = self.page_cursor(
            cursor,
            "search",
            Some(commit),
            None,
            None,
            None,
            Some(&query_digest),
            limit,
        )?;
        let out = self.run_complete(
            &[
                "grep",
                "-I",
                "-n",
                "--full-name",
                "-F",
                "-z",
                "-e",
                query,
                "--end-of-options",
                commit,
                "--",
            ],
            None,
            SEARCH_OUTPUT,
            deadline,
            &[0, 1],
        )?;
        let prefix = format!("{commit}:");
        let (all, content_limited) = parse_search(&out.stdout, &prefix)?;
        let (matches, next, complete, limit_state) = self.page(
            all,
            page_cursor.offset,
            limit,
            out.output_limited || content_limited,
            page_cursor,
        )?;
        Ok(GitSearchPageV1 {
            version: VERSION,
            repository_id: self.repository_id.clone(),
            commit: commit.to_owned(),
            tree,
            query: query.to_owned(),
            matches,
            next,
            complete,
            limit_state,
        })
    }

    fn resolve_ref(&self, selected_ref: &str, deadline: Instant) -> Result<String> {
        let expression = format!("{selected_ref}^{{commit}}");
        let resolved = self.run_complete(
            &["rev-parse", "--verify", "--end-of-options", &expression],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0],
        )?;
        if resolved.output_limited {
            return Err(Error::Limit("Git ref response exceeded its bound".into()));
        }
        let commit = trim_line(&resolved.stdout)?.to_owned();
        validate_oid(&commit, self.object_format)?;
        Ok(commit)
    }

    fn observed_head(&self, deadline: Instant) -> Result<Option<String>> {
        self.run_complete(
            &["rev-parse", "--verify", "--end-of-options", "HEAD^{commit}"],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0, 1, 128],
        )
        .and_then(|out| match (out.code, out.output_limited) {
            (_, true) => Err(Error::Limit("Git HEAD response exceeded its bound".into())),
            (Some(0), false) => {
                let oid = trim_line(&out.stdout)?.to_owned();
                validate_oid(&oid, self.object_format)?;
                Ok(Some(oid))
            }
            _ => Ok(None),
        })
    }

    fn head_ref(&self, deadline: Instant) -> Result<Option<String>> {
        self.run_complete(
            &["symbolic-ref", "--quiet", "HEAD"],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0, 1],
        )
        .and_then(|out| match (out.code, out.output_limited) {
            (_, true) => Err(Error::Limit("Git HEAD response exceeded its bound".into())),
            (Some(0), false) => Ok(Some(trim_line(&out.stdout)?.to_owned())),
            _ => Ok(None),
        })
    }

    fn verify_symbolic_head(
        &self,
        head_ref: Option<&str>,
        observed_head: Option<&str>,
        deadline: Instant,
    ) -> Result<()> {
        let Some(head_ref) = head_ref else {
            return Ok(());
        };
        validate_ref(head_ref)?;
        let expression = format!("{head_ref}^{{commit}}");
        let out = self.run_complete(
            &["rev-parse", "--verify", "--end-of-options", &expression],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0, 1, 128],
        )?;
        if out.output_limited {
            return Err(Error::Limit("Git HEAD response exceeded its bound".into()));
        }
        let resolved = match out.code {
            Some(0) => {
                let oid = trim_line(&out.stdout)?;
                validate_oid(oid, self.object_format)?;
                Some(oid)
            }
            _ => None,
        };
        if resolved != observed_head {
            return Err(Error::Conflict(
                "repository HEAD changed during summary".into(),
            ));
        }
        Ok(())
    }

    fn worktree_observation(
        &self,
        observed_head: Option<&str>,
        deadline: Instant,
    ) -> Result<WorktreeObservation> {
        let staged = match observed_head {
            Some(head) => self.run_complete(
                &[
                    "diff-index",
                    "--cached",
                    "--raw",
                    "-z",
                    "--no-renames",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--ignore-submodules=none",
                    "--end-of-options",
                    head,
                    "--",
                ],
                None,
                STATUS_OUTPUT,
                deadline,
                &[0],
            )?,
            None => self.run_complete(
                &["ls-files", "--cached", "-z", "--"],
                None,
                STATUS_OUTPUT,
                deadline,
                &[0],
            )?,
        };
        let untracked = self.run_complete(
            &["ls-files", "--others", "--exclude-standard", "-z", "--"],
            None,
            STATUS_OUTPUT,
            deadline,
            &[0],
        )?;
        let observed_change = (!staged.output_limited && !staged.stdout.is_empty())
            || (!untracked.output_limited && !untracked.stdout.is_empty());
        Ok(WorktreeObservation {
            dirty: observed_change.then_some(true),
            staged: normalize_observation(staged),
            untracked: normalize_observation(untracked),
        })
    }

    fn refs(&self, deadline: Instant) -> Result<(Vec<GitRefV1>, bool)> {
        let count = format!("--count={}", MAX_REFS + 1);
        let out = self.run_complete(
            &[
                "for-each-ref",
                "--sort=refname",
                count.as_str(),
                "--format=%(refname)%00%(objecttype)%00%(objectname)%00%(*objecttype)%00%(*objectname)",
                "refs/heads",
                "refs/tags",
                "refs/remotes",
            ],
            None,
            REFS_OUTPUT,
            deadline,
            &[0],
        )?;
        if out.output_limited {
            return Err(Error::Limit(
                "repository refs exceed the output bound".into(),
            ));
        }
        let mut refs = parse_refs(&out.stdout, self.object_format)?;
        let omitted = refs.len() > MAX_REFS;
        refs.truncate(MAX_REFS);
        Ok((refs, omitted))
    }

    fn commit_tree(&self, commit: &str, deadline: Instant) -> Result<String> {
        validate_oid(commit, self.object_format)?;
        let kind = self.run_complete(
            &["cat-file", "-t", "--", commit],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0, 1, 128],
        )?;
        if kind.output_limited {
            return Err(Error::Limit(
                "Git object response exceeded its bound".into(),
            ));
        }
        if kind.code != Some(0) || trim_line(&kind.stdout).ok() != Some("commit") {
            return Err(Error::NotFound("commit object".into()));
        }
        let expression = format!("{commit}^{{tree}}");
        let tree = self.run_complete(
            &["rev-parse", "--verify", "--end-of-options", &expression],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0],
        )?;
        let tree = trim_line(&tree.stdout)?.to_owned();
        validate_oid(&tree, self.object_format)?;
        Ok(tree)
    }

    fn tree_entry(
        &self,
        commit: &str,
        path: &str,
        deadline: Instant,
    ) -> Result<Option<RawTreeEntry>> {
        validate_path(path, false)?;
        let out = self.run_complete(
            &["ls-tree", "-z", "-l", "--full-name", commit, "--", path],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0],
        )?;
        if out.output_limited {
            return Err(Error::Limit(
                "repository path lookup exceeded its bound".into(),
            ));
        }
        let mut entries = parse_tree(&out.stdout, self.object_format)?;
        if entries.len() > 1 {
            return Err(Error::Invalid(
                "repository path lookup was not exact".into(),
            ));
        }
        match entries.pop() {
            Some(entry) if entry.path == path => Ok(Some(entry)),
            Some(_) => Err(Error::Invalid(
                "repository path lookup was not exact".into(),
            )),
            None => Ok(None),
        }
    }

    fn blob(&self, oid: &str, deadline: Instant) -> Result<Option<Vec<u8>>> {
        validate_oid(oid, self.object_format)?;
        let out = self.run_complete(
            &["cat-file", "blob", "--", oid],
            None,
            MAX_FILE_BYTES + 1,
            deadline,
            &[0, 1, 128],
        )?;
        if out.code != Some(0) || out.output_limited {
            return Ok(None);
        }
        Ok(Some(out.stdout))
    }

    fn patch(&self, left: &str, right: &str, path: &str, deadline: Instant) -> Result<GitPatchV1> {
        let left_entry = self.tree_entry(left, path, deadline)?;
        let right_entry = self.tree_entry(right, path, deadline)?;
        if left_entry.is_none() && right_entry.is_none() {
            return Ok(patch_state(GitPatchStateV1::Unavailable));
        }
        if left_entry.iter().chain(right_entry.iter()).any(|entry| {
            matches!(
                entry.kind,
                GitEntryKindV1::Directory | GitEntryKindV1::Symlink | GitEntryKindV1::Gitlink
            )
        }) {
            return Ok(patch_state(GitPatchStateV1::Unsupported));
        }
        if left_entry
            .iter()
            .chain(right_entry.iter())
            .any(|entry| entry.size.is_none_or(|size| size > MAX_FILE_BYTES))
        {
            return Ok(patch_state(GitPatchStateV1::TooLarge));
        }
        let left_bytes = match left_entry {
            Some(entry) => match self.blob(&entry.oid, deadline)? {
                Some(bytes) => bytes,
                None => return Ok(patch_state(GitPatchStateV1::Unavailable)),
            },
            None => Vec::new(),
        };
        let right_bytes = match right_entry {
            Some(entry) => match self.blob(&entry.oid, deadline)? {
                Some(bytes) => bytes,
                None => return Ok(patch_state(GitPatchStateV1::Unavailable)),
            },
            None => Vec::new(),
        };
        if left_bytes.contains(&0) || right_bytes.contains(&0) {
            return Ok(patch_state(GitPatchStateV1::Binary));
        }
        if std::str::from_utf8(&left_bytes).is_err() {
            return Ok(patch_state(GitPatchStateV1::Binary));
        }
        if std::str::from_utf8(&right_bytes).is_err() {
            return Ok(patch_state(GitPatchStateV1::Binary));
        }
        let out = self.run_complete(
            &[
                "-c",
                "diff.suppressBlankEmpty=false",
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--no-color",
                "--unified=3",
                "--output-indicator-new=+",
                "--output-indicator-old=-",
                "--output-indicator-context= ",
                "--end-of-options",
                left,
                right,
                "--",
                path,
            ],
            None,
            MAX_PATCH_BYTES as u64 + 1,
            deadline,
            &[0],
        )?;
        if out.output_limited {
            return Ok(patch_state(GitPatchStateV1::TooLarge));
        }
        if out.stdout.contains(&0) {
            return Ok(patch_state(GitPatchStateV1::Binary));
        }
        let Ok(content) = String::from_utf8(out.stdout) else {
            return Ok(patch_state(GitPatchStateV1::Binary));
        };
        if content
            .lines()
            .any(|line| line.starts_with("Binary files ") || line == "GIT binary patch")
        {
            return Ok(patch_state(GitPatchStateV1::Binary));
        }
        if content.len() > MAX_PATCH_BYTES || json_string_size(&content) > MAX_JSON_CONTENT_BYTES {
            return Ok(patch_state(GitPatchStateV1::TooLarge));
        }
        Ok(GitPatchV1 {
            state: GitPatchStateV1::Text,
            content: Some(content),
            truncated: false,
        })
    }

    fn is_shallow(&self, deadline: Instant) -> Result<bool> {
        let out = self.run_complete(
            &["rev-parse", "--is-shallow-repository"],
            None,
            SMALL_OUTPUT,
            deadline,
            &[0],
        )?;
        match trim_line(&out.stdout)? {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(Error::Invalid(
                "Git returned an invalid shallow-state response".into(),
            )),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn page_cursor(
        &self,
        encoded: Option<&str>,
        operation: &str,
        commit: Option<&str>,
        left: Option<&str>,
        right: Option<&str>,
        path: Option<&str>,
        query_digest: Option<&str>,
        limit: usize,
    ) -> Result<PageCursor> {
        let expected = PageCursor {
            version: VERSION,
            repository_id: self.repository_id.clone(),
            operation: operation.to_owned(),
            commit: commit.map(str::to_owned),
            left: left.map(str::to_owned),
            right: right.map(str::to_owned),
            path_digest: path.map(|path| digest(path.as_bytes())),
            query_digest: query_digest.map(str::to_owned),
            limit,
            offset: 0,
        };
        let Some(encoded) = encoded else {
            return Ok(expected);
        };
        let decoded = decode_cursor(encoded)?;
        if decoded.offset > MAX_CURSOR_OFFSET {
            return Err(Error::Limit(
                "explorer cursor offset exceeds its bound".into(),
            ));
        }
        if decoded.version != expected.version
            || decoded.repository_id != expected.repository_id
            || decoded.operation != expected.operation
            || decoded.commit != expected.commit
            || decoded.left != expected.left
            || decoded.right != expected.right
            || decoded.path_digest != expected.path_digest
            || decoded.query_digest != expected.query_digest
            || decoded.limit != expected.limit
        {
            return Err(Error::Invalid(
                "explorer cursor does not match the request".into(),
            ));
        }
        Ok(decoded)
    }

    fn page<T>(
        &self,
        all: Vec<T>,
        offset: usize,
        limit: usize,
        output_limited: bool,
        mut cursor: PageCursor,
    ) -> Result<(Vec<T>, Option<String>, bool, PageLimitStateV1)> {
        if offset > all.len() && !output_limited {
            return Err(Error::Invalid(
                "explorer cursor is past the immutable result".into(),
            ));
        }
        let available = all.len().saturating_sub(offset);
        let take = available.min(limit);
        let entries = all.into_iter().skip(offset).take(take).collect::<Vec<_>>();
        if output_limited {
            return Ok((entries, None, false, PageLimitStateV1::OutputLimited));
        }
        if available > limit {
            cursor.offset = offset
                .checked_add(take)
                .ok_or_else(|| Error::Limit("explorer cursor offset is invalid".into()))?;
            if cursor.offset > MAX_CURSOR_OFFSET {
                return Err(Error::Limit(
                    "explorer cursor offset exceeds its bound".into(),
                ));
            }
            return Ok((
                entries,
                Some(encode_cursor(&cursor)?),
                false,
                PageLimitStateV1::MoreResults,
            ));
        }
        Ok((entries, None, true, PageLimitStateV1::Complete))
    }

    fn run_complete(
        &self,
        args: &[&str],
        input: Option<Vec<u8>>,
        output_bound: u64,
        deadline: Instant,
        accepted_codes: &[i32],
    ) -> Result<GitOutput> {
        self.ensure_pinned()?;
        let timeout = deadline.saturating_duration_since(Instant::now());
        if timeout.is_zero() {
            return Err(Error::Limit("Git explorer request timed out".into()));
        }
        let mut command = Command::new(&self.git);
        command
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .env("GIT_NO_LAZY_FETCH", "1")
            .env("GIT_PAGER", "cat")
            .env("PAGER", "cat")
            .env("LC_ALL", "C")
            .env("HOME", "/nonexistent")
            .process_group(0)
            .arg("-C")
            .arg(&self.root)
            .args([
                "--no-pager",
                "--no-replace-objects",
                "--no-lazy-fetch",
                "--no-optional-locks",
                "--literal-pathspecs",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
                "-c",
                "core.attributesFile=/dev/null",
                "-c",
                "diff.external=",
                "-c",
                "diff.trustExitCode=false",
            ])
            .args(args);
        let capture = capture_with_input(
            &mut command,
            input,
            timeout,
            output_bound,
            &Cancellation::default(),
        )?;
        self.ensure_pinned()?;
        match capture.stop {
            Some(Stop::Output) if !capture.incomplete => Ok(GitOutput {
                code: capture.code,
                stdout: capture.stdout,
                output_limited: true,
            }),
            Some(Stop::Timeout) => Err(Error::Limit("Git explorer request timed out".into())),
            Some(Stop::Output) | Some(Stop::Cancelled) | Some(Stop::Io) => Err(Error::Invalid(
                "Git explorer subprocess did not complete".into(),
            )),
            None if capture
                .code
                .is_some_and(|code| accepted_codes.contains(&code)) =>
            {
                Ok(GitOutput {
                    code: capture.code,
                    stdout: capture.stdout,
                    output_limited: false,
                })
            }
            None => Err(Error::Invalid("Git explorer subprocess failed".into())),
        }
    }

    fn ensure_pinned(&self) -> Result<()> {
        if !self.root_pin.matches(&self.root)
            || !self.git_metadata_pin.matches(&self.git_metadata_path)
            || !self.git_pin.matches(&self.git)
            || self
                .git_dir
                .as_ref()
                .is_some_and(|(path, pin)| !pin.matches(path))
            || self
                .git_common_dir
                .as_ref()
                .is_some_and(|(path, pin)| !pin.matches(path))
        {
            return Err(Error::Conflict(
                "configured repository identity changed".into(),
            ));
        }
        if let Some(expected) = &self.git_metadata_contents {
            let current = fs::read(&self.git_metadata_path)
                .map_err(|_| Error::Conflict("configured repository identity changed".into()))?;
            if &current != expected {
                return Err(Error::Conflict(
                    "configured repository identity changed".into(),
                ));
            }
        }
        Ok(())
    }
}

fn deadline() -> Instant {
    Instant::now() + REQUEST_DEADLINE
}

fn normalize_observation(mut output: GitOutput) -> GitOutput {
    if output.output_limited {
        output.code = None;
        output.stdout.clear();
    }
    output
}

fn patch_state(state: GitPatchStateV1) -> GitPatchV1 {
    GitPatchV1 {
        state,
        content: None,
        truncated: false,
    }
}

pub(crate) fn find_git() -> Result<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            if directory.is_absolute() {
                candidates.push(directory.join("git"));
            }
        }
    }
    candidates.extend([
        PathBuf::from("/usr/bin/git"),
        PathBuf::from("/usr/local/bin/git"),
    ]);
    for candidate in candidates {
        let Ok(metadata) = fs::metadata(&candidate) else {
            continue;
        };
        if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 {
            let canonical = fs::canonicalize(candidate)
                .map_err(|_| Error::Invalid("Git executable is unavailable".into()))?;
            if canonical.is_absolute() {
                return Ok(canonical);
            }
        }
    }
    Err(Error::Invalid("Git executable is unavailable".into()))
}

fn validate_repository_id(repository_id: &str) -> Result<()> {
    if repository_id.is_empty()
        || repository_id.len() > MAX_REPOSITORY_ID_BYTES
        || repository_id.trim() != repository_id
        || repository_id.chars().any(char::is_control)
    {
        return Err(Error::Invalid("repository ID is invalid".into()));
    }
    Ok(())
}

fn validate_ref(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_REF_BYTES
        || value.as_bytes().contains(&0)
        || value.contains(['\r', '\n'])
    {
        return Err(Error::Invalid("Git ref is invalid".into()));
    }
    Ok(())
}

fn validate_oid(value: &str, format: ObjectFormat) -> Result<()> {
    if value.len() != format.oid_len()
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid(
            "full lowercase Git object ID required".into(),
        ));
    }
    Ok(())
}

fn validate_path(path: &str, allow_empty: bool) -> Result<&str> {
    if path.len() > MAX_PATH_BYTES
        || path.as_bytes().contains(&0)
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || (!allow_empty && path.is_empty())
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(Error::Invalid("repository path is invalid".into()));
    }
    Ok(path)
}

fn normalize_optional_path(path: Option<&str>) -> Result<Option<String>> {
    match path {
        None | Some("") => Ok(None),
        Some(path) => Ok(Some(validate_path(path, false)?.to_owned())),
    }
}

fn validate_query(query: &str) -> Result<()> {
    if query.is_empty()
        || query.len() > MAX_QUERY_BYTES
        || query
            .chars()
            .any(|character| matches!(character, '\0' | '\r' | '\n'))
    {
        return Err(Error::Invalid("search query is invalid".into()));
    }
    Ok(())
}

fn validate_limit(limit: usize, maximum: usize, operation: &str) -> Result<()> {
    if !(1..=maximum).contains(&limit) {
        return Err(Error::Limit(format!(
            "{operation} page limit must be between 1 and {maximum}"
        )));
    }
    Ok(())
}

fn trim_line(bytes: &[u8]) -> Result<&str> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if bytes.contains(&b'\n') || bytes.contains(&b'\r') || bytes.contains(&0) {
        return Err(Error::Invalid("Git returned malformed text".into()));
    }
    std::str::from_utf8(bytes).map_err(|_| Error::Invalid("Git returned non-UTF-8 text".into()))
}

fn path_from_line(bytes: &[u8]) -> Result<PathBuf> {
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'\n') || bytes.contains(&b'\r') {
        return Err(Error::Invalid(
            "Git returned an invalid repository path".into(),
        ));
    }
    Ok(PathBuf::from(OsString::from_vec(bytes.to_vec())))
}

fn canonical_git_directory(bytes: &[u8], root: &Path) -> Result<PathBuf> {
    let path = path_from_line(bytes)?;
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    fs::canonicalize(path)
        .map_err(|_| Error::Invalid("configured Git metadata directory is unavailable".into()))
}

fn unix_seconds() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| Error::Invalid("system clock is before the Unix epoch".into()))?
        .as_secs();
    i64::try_from(seconds).map_err(|_| Error::Limit("system timestamp exceeds range".into()))
}

fn digest(bytes: &[u8]) -> String {
    let value = Sha256::digest(bytes);
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn encode_cursor(cursor: &PageCursor) -> Result<String> {
    let body = serde_json::to_vec(cursor)?;
    let encoded = URL_SAFE_NO_PAD.encode(&body);
    let value = format!("{}.{encoded}", digest(&body));
    if value.len() > MAX_CURSOR_BYTES {
        return Err(Error::Limit(
            "explorer cursor exceeds its byte bound".into(),
        ));
    }
    Ok(value)
}

fn decode_cursor(value: &str) -> Result<PageCursor> {
    if value.len() > MAX_CURSOR_BYTES {
        return Err(Error::Limit(
            "explorer cursor exceeds its byte bound".into(),
        ));
    }
    let (checksum, encoded) = value
        .split_once('.')
        .ok_or_else(|| Error::Invalid("explorer cursor is malformed".into()))?;
    if checksum.len() != 64
        || !checksum
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Error::Invalid("explorer cursor is malformed".into()));
    }
    let body = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| Error::Invalid("explorer cursor is malformed".into()))?;
    if digest(&body) != checksum {
        return Err(Error::Invalid("explorer cursor checksum mismatch".into()));
    }
    serde_json::from_slice(&body).map_err(|_| Error::Invalid("explorer cursor is malformed".into()))
}

fn parse_tree(bytes: &[u8], format: ObjectFormat) -> Result<Vec<RawTreeEntry>> {
    let mut entries = Vec::new();
    for record in complete_nul_records(bytes) {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| Error::Invalid("Git returned a malformed tree row".into()))?;
        let header = std::str::from_utf8(&record[..tab])
            .map_err(|_| Error::Invalid("Git returned a malformed tree row".into()))?;
        let path = std::str::from_utf8(&record[tab + 1..])
            .map_err(|_| Error::Invalid("repository path is not UTF-8".into()))?;
        validate_path(path, false)?;
        let fields = header.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() != 4 {
            return Err(Error::Invalid("Git returned a malformed tree row".into()));
        }
        let mode = fields[0];
        let object_type = fields[1];
        let oid = fields[2];
        validate_oid(oid, format)?;
        let kind = match (mode, object_type) {
            ("040000", "tree") => GitEntryKindV1::Directory,
            ("100644", "blob") => GitEntryKindV1::File,
            ("100755", "blob") => GitEntryKindV1::Executable,
            ("120000", "blob") => GitEntryKindV1::Symlink,
            ("160000", "commit") => GitEntryKindV1::Gitlink,
            _ => return Err(Error::Invalid("repository tree mode is unsupported".into())),
        };
        let size = match fields[3] {
            "-" => None,
            size => Some(
                size.parse::<u64>()
                    .map_err(|_| Error::Invalid("Git returned an invalid blob size".into()))?,
            ),
        };
        if matches!(kind, GitEntryKindV1::Directory | GitEntryKindV1::Gitlink) && size.is_some() {
            return Err(Error::Invalid("Git returned an invalid tree size".into()));
        }
        entries.push(RawTreeEntry {
            path: path.to_owned(),
            kind,
            oid: oid.to_owned(),
            size,
        });
    }
    Ok(entries)
}

fn complete_nul_records(bytes: &[u8]) -> impl Iterator<Item = &[u8]> {
    let complete = bytes
        .iter()
        .rposition(|byte| *byte == 0)
        .map_or(&bytes[..0], |end| &bytes[..=end]);
    complete
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
}

fn parse_refs(bytes: &[u8], format: ObjectFormat) -> Result<Vec<GitRefV1>> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Invalid("Git returned malformed refs".into()))?;
    let mut refs = Vec::new();
    for line in text.lines() {
        if line.is_empty() {
            continue;
        }
        let fields = line.split('\0').collect::<Vec<_>>();
        if fields.len() != 5 {
            return Err(Error::Invalid("Git returned a malformed ref row".into()));
        }
        let name = fields[0];
        if name.len() > MAX_REF_BYTES || name.chars().any(char::is_control) {
            return Err(Error::Invalid("Git returned an invalid ref name".into()));
        }
        let commit = if fields[1] == "commit" {
            fields[2]
        } else if fields[3] == "commit" {
            fields[4]
        } else {
            continue;
        };
        validate_oid(commit, format)?;
        let kind = if name.starts_with("refs/heads/") {
            GitRefKindV1::LocalBranch
        } else if name.starts_with("refs/tags/") {
            GitRefKindV1::Tag
        } else if name.starts_with("refs/remotes/") {
            GitRefKindV1::RemoteTracking
        } else {
            continue;
        };
        refs.push(GitRefV1 {
            name: name.to_owned(),
            commit: commit.to_owned(),
            kind,
        });
    }
    Ok(refs)
}

fn parse_history(bytes: &[u8], format: ObjectFormat) -> Result<(Vec<GitHistoryEntryV1>, bool)> {
    let mut entries = Vec::new();
    let mut content_limited = false;
    let tokens = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut index = 0;
    while index + 5 < tokens.len() {
        let mut oid_bytes = tokens[index];
        if let Some(stripped) = oid_bytes.strip_prefix(b"\n") {
            oid_bytes = stripped;
        }
        if oid_bytes.is_empty() {
            break;
        }
        if !tokens[index + 5].is_empty() {
            return Err(Error::Invalid(
                "Git returned a malformed history row".into(),
            ));
        }
        let oid = ascii(oid_bytes, "history object ID")?;
        validate_oid(oid, format)?;
        let parents_text = ascii(tokens[index + 1], "history parents")?;
        let mut parents = Vec::new();
        if !parents_text.is_empty() {
            for parent in parents_text.split(' ') {
                if parents.len() == MAX_COMMIT_PARENTS {
                    content_limited = true;
                    break;
                }
                validate_oid(parent, format)?;
                parents.push(parent.to_owned());
            }
        }
        let author_name = String::from_utf8_lossy(tokens[index + 2]);
        let author_name = truncate_json_string(&author_name, MAX_HISTORY_NAME_BYTES);
        let author_time = ascii(tokens[index + 3], "author timestamp")?
            .parse::<i64>()
            .map_err(|_| Error::Invalid("Git returned an invalid author timestamp".into()))?;
        let subject = String::from_utf8_lossy(tokens[index + 4]);
        let subject = truncate_json_string(&subject, MAX_HISTORY_SUBJECT_BYTES);
        if parents_text
            .split(' ')
            .filter(|parent| !parent.is_empty())
            .count()
            > MAX_COMMIT_PARENTS
        {
            content_limited = true;
            index += 6;
            continue;
        }
        entries.push(GitHistoryEntryV1 {
            oid: oid.to_owned(),
            parents,
            author_name,
            author_time,
            subject,
        });
        index += 6;
    }
    Ok((entries, content_limited))
}

fn parse_diff(
    bytes: &[u8],
    format: ObjectFormat,
    allow_truncated: bool,
) -> Result<Vec<GitDiffChangeV1>> {
    let mut tokens = bytes.split(|byte| *byte == 0).peekable();
    let mut changes = Vec::new();
    while let Some(header) = tokens.next() {
        if header.is_empty() {
            continue;
        }
        let header = ascii(header, "diff header")?;
        let fields = header.split_ascii_whitespace().collect::<Vec<_>>();
        if fields.len() != 5 || !fields[0].starts_with(':') {
            return Err(Error::Invalid("Git returned a malformed diff row".into()));
        }
        let old_mode = fields[0]
            .strip_prefix(':')
            .ok_or_else(|| Error::Invalid("Git returned a malformed diff mode".into()))?;
        let mode = fields[1];
        validate_mode(old_mode)?;
        validate_mode(mode)?;
        let old_oid = optional_oid(fields[2], format)?;
        let oid = optional_oid(fields[3], format)?;
        let status = fields[4];
        let status_byte = status
            .as_bytes()
            .first()
            .copied()
            .ok_or_else(|| Error::Invalid("Git returned an empty diff status".into()))?;
        let similarity = if matches!(status_byte, b'R' | b'C') {
            let value = status[1..]
                .parse::<u8>()
                .map_err(|_| Error::Invalid("Git returned an invalid similarity".into()))?;
            if value > 100 {
                return Err(Error::Invalid("Git returned an invalid similarity".into()));
            }
            Some(value)
        } else {
            if status.len() != 1 {
                return Err(Error::Invalid(
                    "Git returned a malformed diff status".into(),
                ));
            }
            None
        };
        let Some(first) = tokens.next().filter(|token| !token.is_empty()) else {
            if allow_truncated {
                break;
            }
            return Err(Error::Invalid("Git returned a truncated diff row".into()));
        };
        let first = utf8_path(first)?;
        let (old_path, path) = if matches!(status_byte, b'R' | b'C') {
            let Some(second) = tokens.next().filter(|token| !token.is_empty()) else {
                if allow_truncated {
                    break;
                }
                return Err(Error::Invalid("Git returned a truncated rename row".into()));
            };
            (Some(first), Some(utf8_path(second)?))
        } else if status_byte == b'A' {
            (None, Some(first))
        } else if status_byte == b'D' {
            (Some(first), None)
        } else {
            (Some(first.clone()), Some(first))
        };
        let kind = match status_byte {
            b'A' => GitDiffKindV1::Added,
            b'D' => GitDiffKindV1::Deleted,
            b'M' => GitDiffKindV1::Modified,
            b'R' => GitDiffKindV1::Renamed,
            b'C' => GitDiffKindV1::Copied,
            b'T' => GitDiffKindV1::TypeChanged,
            b'U' => GitDiffKindV1::Unmerged,
            _ => GitDiffKindV1::Unknown,
        };
        changes.push(GitDiffChangeV1 {
            old_path,
            path,
            kind,
            similarity,
            old_oid,
            oid,
            old_mode: (old_mode != "000000").then(|| old_mode.to_owned()),
            mode: (mode != "000000").then(|| mode.to_owned()),
        });
    }
    Ok(changes)
}

fn parse_search(bytes: &[u8], prefix: &str) -> Result<(Vec<GitSearchMatchV1>, bool)> {
    let mut matches = Vec::new();
    let mut content_limited = false;
    let mut remaining = bytes;
    while let Some(path_end) = remaining.iter().position(|byte| *byte == 0) {
        let raw_path = &remaining[..path_end];
        remaining = &remaining[path_end + 1..];
        let Some(line_end) = remaining.iter().position(|byte| *byte == 0) else {
            break;
        };
        let raw_line = &remaining[..line_end];
        remaining = &remaining[line_end + 1..];
        let Some(text_end) = remaining.iter().position(|byte| *byte == b'\n') else {
            break;
        };
        let mut raw_text = &remaining[..text_end];
        remaining = &remaining[text_end + 1..];
        if raw_text.ends_with(b"\r") {
            raw_text = &raw_text[..raw_text.len() - 1];
        }
        let shown_path = std::str::from_utf8(raw_path)
            .map_err(|_| Error::Invalid("repository path is not UTF-8".into()))?;
        let path = shown_path
            .strip_prefix(prefix)
            .ok_or_else(|| Error::Invalid("Git returned an unbound search path".into()))?;
        validate_path(path, false)?;
        let line = ascii(raw_line, "search line number")?
            .parse::<u64>()
            .map_err(|_| Error::Invalid("Git returned an invalid search line".into()))?;
        if line == 0 {
            return Err(Error::Invalid("Git returned an invalid search line".into()));
        }
        let Ok(text) = std::str::from_utf8(raw_text) else {
            continue;
        };
        if json_string_size(text) > MAX_SEARCH_TEXT_BYTES {
            content_limited = true;
            continue;
        }
        matches.push(GitSearchMatchV1 {
            path: path.to_owned(),
            line,
            text: text.to_owned(),
        });
    }
    Ok((matches, content_limited))
}

fn json_string_size(value: &str) -> usize {
    value
        .chars()
        .map(|character| match character {
            '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' | '"' | '\\' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        })
        .sum::<usize>()
        .saturating_add(2)
}

fn truncate_json_string(value: &str, maximum: usize) -> String {
    const MARKER: &str = " [truncated]";
    if json_string_size(value) <= maximum {
        return value.to_owned();
    }
    let budget = maximum.saturating_sub(json_string_size(MARKER));
    let mut truncated = String::new();
    let mut encoded_bytes = 2usize;
    for character in value.chars() {
        let encoded = match character {
            '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' | '"' | '\\' => 2,
            '\u{0000}'..='\u{001f}' => 6,
            _ => character.len_utf8(),
        };
        if encoded_bytes.saturating_add(encoded) > budget {
            break;
        }
        truncated.push(character);
        encoded_bytes += encoded;
    }
    truncated.push_str(MARKER);
    truncated
}

fn ascii<'a>(bytes: &'a [u8], label: &str) -> Result<&'a str> {
    if !bytes.is_ascii() {
        return Err(Error::Invalid(format!("Git returned an invalid {label}")));
    }
    Ok(std::str::from_utf8(bytes).expect("ASCII is UTF-8"))
}

fn utf8_path(bytes: &[u8]) -> Result<String> {
    let path = std::str::from_utf8(bytes)
        .map_err(|_| Error::Invalid("repository path is not UTF-8".into()))?;
    validate_path(path, false)?;
    Ok(path.to_owned())
}

fn optional_oid(value: &str, format: ObjectFormat) -> Result<Option<String>> {
    if value.len() == format.oid_len() && value.bytes().all(|byte| byte == b'0') {
        return Ok(None);
    }
    validate_oid(value, format)?;
    Ok(Some(value.to_owned()))
}

fn validate_mode(mode: &str) -> Result<()> {
    if mode.len() != 6 || !mode.bytes().all(|byte| (b'0'..=b'7').contains(&byte)) {
        return Err(Error::Invalid("Git returned an invalid file mode".into()));
    }
    Ok(())
}
