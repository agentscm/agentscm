//! Source selection for workspace capture: which working-tree paths are part of the source.
//!
//! Rules use gitignore syntax and come from, lowest to highest precedence: the user's global Git
//! excludes and `$GIT_DIR/info/exclude` (only when the workspace root has a `.git`), `.gitignore`
//! files (deeper beats shallower), the tracked `.agentscmignore` at the root, and the private
//! `.agentscm-workspace/exclude`. The highest source with any matching rule decides, and within a
//! source the last matching rule wins, so a `!pattern` in a higher source re-includes a path.
//!
//! Paths present in the base revision (and their parent directories) are always included, so a
//! tracked edit or deletion is never silently hidden. An excluded directory is not descended into
//! and its contents cannot be re-included (pinned by `a_parent_exclusion_cannot_be_re_included_by_a_higher_source`).

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::fs;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::Digest;

use super::glob::{Rule as GlobRule, basename, parse_line};
use crate::{Error, Result};

/// Largest accepted rule file (`.gitignore`, `info/exclude`, ...). Larger files are refused
/// rather than truncated, because a truncated rule list would select the wrong paths. The caps
/// bound reading and pattern count only: matcher memory for pathological patterns within them
/// is not qualified.
pub const MAX_RULE_FILE_BYTES: u64 = 64 * 1024;
/// Cumulative work in one scan over all rule sources; never credited back.
pub const MAX_RULE_WORK_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_RULE_WORK_PATTERNS: usize = 100_000;
/// Cap on matching work in one scan, checked after every rule evaluation. A unit is one byte of
/// work: per rule considered, 1 plus the suffix bytes compared; per path and rule file, the
/// path length; and per evaluation that runs the matching program, tokens x (subject length +
/// 1). Measured in release builds at roughly 0.3 ns per unit for pre-checks and 0.7 ns per unit
/// for star-heavy matching, so the cap is a few seconds of matching at most (slower machines and
/// debug builds take longer). Ordinary workloads stay far below it: 6,000 `*.eN` rules over
/// 20,000 paths use 8.2 x 10^8 units, and 150 mixed rules over 100,000 paths use 6.1 x 10^8.
pub const MAX_MATCH_WORK: u64 = 3_000_000_000;
/// Rules resident at once (the `.gitignore` files of the directories being walked, plus the
/// fixed sources); credited back when the walk leaves a directory.
pub const MAX_LIVE_RULE_BYTES: u64 = 1024 * 1024;
pub const MAX_LIVE_RULE_PATTERNS: usize = 10_000;

pub const PRIVATE_EXCLUDE_PATH: &str = ".agentscm-workspace/exclude";
pub const TRACKED_IGNORE_FILE: &str = ".agentscmignore";
pub const GLOBAL_EXCLUDES_SOURCE: &str = "global-excludes";
pub const INFO_EXCLUDE_SOURCE: &str = "info/exclude";

/// The rule that decided a path: where it came from and its original text.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Rule {
    pub source: String,
    pub pattern: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    /// No rule excluded the path.
    Included,
    /// A `!pattern` rule explicitly included the path.
    IncludedByRule(Rule),
    Excluded(Rule),
}

/// A selection plus the exclusion its children inherit when a tracked directory is walked
/// although a rule excludes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Classified {
    pub selection: Selection,
    pub child_exclusion: Option<Rule>,
    /// The exclusion rule that tracking overrode, if any.
    pub overridden: Option<Rule>,
}

/// Paths of the base revision; they are always selected.
#[derive(Clone, Debug, Default)]
pub struct Tracked {
    files: BTreeSet<String>,
    directories: BTreeSet<String>,
}

impl Tracked {
    pub fn from_paths<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut tracked = Self::default();
        for path in paths {
            for (separator, _) in path.match_indices('/') {
                tracked.directories.insert(path[..separator].to_owned());
            }
            tracked.files.insert(path.to_owned());
        }
        tracked
    }

    fn contains(&self, path: &str) -> bool {
        self.files.contains(path) || self.directories.contains(path)
    }
}

struct Layer {
    rules: Vec<GlobRule>,
    /// Directory the rules are relative to, from the workspace root; empty for the root.
    base: String,
    label: String,
    bytes: u64,
    patterns: usize,
}

/// Inputs the selector cannot read from the workspace itself.
#[derive(Clone, Debug, Default)]
pub struct Settings {
    /// Git's common directory (holding `info/exclude`) when the root is a Git work tree, as
    /// reported by Git itself. `None` for a workspace without Git; the Git-only sources below
    /// are then not read.
    pub git_common_dir: Option<PathBuf>,
    /// The user's global Git excludes file; read only with `git_common_dir`.
    pub global_excludes: Option<PathBuf>,
    /// Match patterns case-insensitively (Git's `core.ignorecase`, or the filesystem's case
    /// behavior for a workspace without Git).
    pub case_insensitive: bool,
}

/// How a rule file that is a symbolic link or not a regular file is treated.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum LinkPolicy {
    /// A typed error.
    Refuse,
    /// Followed, still size-bounded and checked to be a regular file after opening.
    Follow,
    /// Not a rule source (an in-tree `.gitignore` symlink is ignored; see
    /// `in_tree_gitignore_links_are_not_rule_sources_and_global_links_are_followed`).
    Skip,
}

#[derive(Default)]
struct Budget {
    work_bytes: u64,
    work_patterns: usize,
    live_bytes: u64,
    live_patterns: usize,
}

impl Budget {
    fn charge_bytes(&mut self, bytes: u64, label: &str) -> Result<()> {
        self.work_bytes += bytes;
        self.live_bytes += bytes;
        if self.work_bytes > MAX_RULE_WORK_BYTES || self.live_bytes > MAX_LIVE_RULE_BYTES {
            return Err(Error::Limit(format!(
                "exclusion rule files exceed their byte budget at {label}; exclude its directory or shorten the file"
            )));
        }
        Ok(())
    }

    fn charge_pattern(&mut self, label: &str) -> Result<()> {
        self.work_patterns += 1;
        self.live_patterns += 1;
        if self.work_patterns > MAX_RULE_WORK_PATTERNS
            || self.live_patterns > MAX_LIVE_RULE_PATTERNS
        {
            return Err(Error::Limit(format!(
                "exclusion rules exceed their pattern budget at {label}; exclude its directory or shorten the file"
            )));
        }
        Ok(())
    }

    fn credit(&mut self, bytes: u64, patterns: usize) {
        self.live_bytes -= bytes;
        self.live_patterns -= patterns;
    }
}

pub struct Selector<'a> {
    inputs: sha2::Sha256,
    tracked: &'a Tracked,
    case_insensitive: bool,
    budget: Budget,
    /// Matching work so far (see [`MAX_MATCH_WORK`]) and the rule file that crossed the limit.
    work: Cell<u64>,
    work_limit: u64,
    exceeded: RefCell<Option<String>>,
    /// Highest precedence first.
    top: Vec<Layer>,
    /// Per-directory `.gitignore` layers, shallowest first.
    stack: Vec<Layer>,
    /// Highest precedence first.
    bottom: Vec<Layer>,
}

impl<'a> Selector<'a> {
    /// Build a selector from the files currently on disk.
    pub fn open(root: &Path, tracked: &'a Tracked, settings: &Settings) -> Result<Self> {
        let mut selector = Self {
            inputs: sha2::Sha256::new(),
            tracked,
            case_insensitive: settings.case_insensitive,
            budget: Budget::default(),
            work: Cell::new(0),
            work_limit: MAX_MATCH_WORK,
            exceeded: RefCell::new(None),
            top: Vec::new(),
            stack: Vec::new(),
            bottom: Vec::new(),
        };
        let private = root
            .join(crate::local_recovery::WORKSPACE_METADATA_DIRECTORY)
            .join("exclude");
        let layers = [
            selector.layer(&private, PRIVATE_EXCLUDE_PATH, "", LinkPolicy::Refuse)?,
            selector.layer(
                &root.join(TRACKED_IGNORE_FILE),
                TRACKED_IGNORE_FILE,
                "",
                LinkPolicy::Refuse,
            )?,
        ];
        selector.top.extend(layers.into_iter().flatten());
        if let Some(git_dir) = &settings.git_common_dir {
            let info = selector.layer(
                &git_dir.join("info").join("exclude"),
                INFO_EXCLUDE_SOURCE,
                "",
                LinkPolicy::Follow,
            )?;
            selector.bottom.extend(info);
            if let Some(global) = &settings.global_excludes {
                let global =
                    selector.layer(global, GLOBAL_EXCLUDES_SOURCE, "", LinkPolicy::Follow)?;
                selector.bottom.extend(global);
            }
        }
        Ok(selector)
    }

    pub fn input_digest(&self) -> String {
        format!("{:x}", self.inputs.clone().finalize())
    }

    fn layer(
        &mut self,
        path: &Path,
        label: &str,
        base: &str,
        links: LinkPolicy,
    ) -> Result<Option<Layer>> {
        let bytes = read_bounded(path, label, MAX_RULE_FILE_BYTES, links)?;
        let path = path.as_os_str().as_bytes();
        self.inputs.update((path.len() as u64).to_be_bytes());
        self.inputs.update(path);
        match &bytes {
            Some(bytes) => {
                self.inputs.update([1]);
                self.inputs.update((bytes.len() as u64).to_be_bytes());
                self.inputs.update(bytes);
            }
            None => self.inputs.update([0]),
        }
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let size = bytes.len() as u64;
        self.budget.charge_bytes(size, label)?;
        let layer = build_layer(&bytes, label, base, self.case_insensitive, &mut self.budget)?;
        match layer {
            Some(mut layer) => {
                layer.bytes = size;
                Ok(Some(layer))
            }
            None => {
                // Nothing stays resident for a file with no patterns.
                self.budget.credit(size, 0);
                Ok(None)
            }
        }
    }

    /// Load `directory/.gitignore` for the walk into `relative`. Returns whether a layer was
    /// pushed, in which case the caller must call [`Selector::leave_directory`].
    pub fn enter_directory(&mut self, directory: &Path, relative: &str) -> Result<bool> {
        let label = if relative.is_empty() {
            ".gitignore".to_owned()
        } else {
            format!("{relative}/.gitignore")
        };
        let Some(layer) = self.layer(
            &directory.join(".gitignore"),
            &label,
            relative,
            LinkPolicy::Skip,
        )?
        else {
            return Ok(false);
        };
        self.stack.push(layer);
        Ok(true)
    }

    pub fn leave_directory(&mut self) {
        if let Some(layer) = self.stack.pop() {
            self.budget.credit(layer.bytes, layer.patterns);
        }
    }

    /// Classify one entry. `inherited` is the exclusion carried by the parent directory.
    pub fn classify(&self, relative: &Path, is_dir: bool, inherited: Option<&Rule>) -> Classified {
        let rule_selection = match inherited {
            Some(rule) => Selection::Excluded(rule.clone()),
            None => self.match_rules(relative, is_dir),
        };
        let tracked = relative
            .to_str()
            .is_some_and(|path| self.tracked.contains(path));
        match rule_selection {
            Selection::Excluded(rule) if tracked => Classified {
                selection: Selection::Included,
                child_exclusion: is_dir.then(|| rule.clone()),
                overridden: Some(rule),
            },
            Selection::Excluded(rule) => Classified {
                child_exclusion: Some(rule.clone()),
                selection: Selection::Excluded(rule),
                overridden: None,
            },
            selection => Classified {
                selection,
                child_exclusion: None,
                overridden: None,
            },
        }
    }

    fn match_rules(&self, relative: &Path, is_dir: bool) -> Selection {
        let path = relative.as_os_str().as_bytes();
        for layer in self
            .top
            .iter()
            .chain(self.stack.iter().rev())
            .chain(self.bottom.iter())
        {
            if self.exceeded.borrow().is_some() {
                return Selection::Included;
            }
            if let Some(selection) = self.probe(layer, path, is_dir) {
                return selection;
            }
        }
        Selection::Included
    }

    /// The last rule of `layer` that matches decides. Work is checked after every rule, so the
    /// source that crosses the cap is the one named, whichever rule or path that is.
    fn probe(&self, layer: &Layer, path: &[u8], is_dir: bool) -> Option<Selection> {
        let relative = if layer.base.is_empty() {
            path
        } else {
            path.strip_prefix(layer.base.as_bytes())?
                .strip_prefix(b"/")?
        };
        let name = basename(relative);
        // Finding the basename reads the path once.
        self.work
            .set(self.work.get().saturating_add(relative.len() as u64));
        for rule in layer.rules.iter().rev() {
            let matched = rule.matches_counted(relative, name, is_dir, &self.work);
            if self.work.get() > self.work_limit {
                self.exceeded
                    .borrow_mut()
                    .get_or_insert_with(|| layer.label.clone());
                return None;
            }
            if matched {
                let reported = Rule {
                    source: layer.label.clone(),
                    pattern: rule.original.clone(),
                };
                return Some(if rule.negated {
                    Selection::IncludedByRule(reported)
                } else {
                    Selection::Excluded(reported)
                });
            }
        }
        None
    }

    /// An error once matching work crossed its cap; classification stops matching at that
    /// point, so the scan must not use its results.
    pub fn check_work(&self) -> Result<()> {
        match self.exceeded.borrow().as_deref() {
            None => Ok(()),
            Some(label) => Err(Error::Limit(format!(
                "matching the exclusion rules in {label} exceeded the work budget; exclude its directory or simplify its patterns"
            ))),
        }
    }

    #[cfg(test)]
    pub fn matching_work(&self) -> u64 {
        self.work.get()
    }

    #[cfg(test)]
    pub fn set_work_limit(&mut self, limit: u64) {
        self.work_limit = limit;
    }
}

fn build_layer(
    bytes: &[u8],
    label: &str,
    base: &str,
    case_insensitive: bool,
    budget: &mut Budget,
) -> Result<Option<Layer>> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let mut rules = Vec::new();
    for (index, line) in text.split('\n').enumerate() {
        // One carriage return directly before the newline (or the end of the file) is part of
        // the line ending; rows `a\r\n`, `a\r\r\n` and `a\r ` of the adversarial table pin this.
        let line = line.strip_suffix('\r').unwrap_or(line);
        let parsed = parse_line(line, case_insensitive).map_err(|()| {
            Error::Invalid(format!(
                "exclusion rule file {label} has an unsupported pattern on line {}",
                index + 1
            ))
        })?;
        let Some(rule) = parsed else {
            continue;
        };
        budget.charge_pattern(label)?;
        rules.push(rule);
    }
    if rules.is_empty() {
        return Ok(None);
    }
    let patterns = rules.len();
    Ok(Some(Layer {
        rules,
        base: base.to_owned(),
        label: label.to_owned(),
        bytes: 0,
        patterns,
    }))
}

/// Read a rule file with a size bound. A missing file is no rules. A path that is not a regular
/// file follows `links`.
pub(super) fn read_bounded(
    path: &Path,
    label: &str,
    limit: u64,
    links: LinkPolicy,
) -> Result<Option<Vec<u8>>> {
    let link_metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => {
            return Ok(None);
        }
        Err(error) => return Err(Error::Io(error)),
    };
    let not_regular = |links: LinkPolicy| -> Result<Option<Vec<u8>>> {
        match links {
            LinkPolicy::Skip => Ok(None),
            _ => Err(Error::Invalid(format!(
                "exclusion rule file is not a regular file: {label}"
            ))),
        }
    };
    let mut metadata = link_metadata;
    if metadata.file_type().is_symlink() && links == LinkPolicy::Follow {
        metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error)
                if matches!(error.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) =>
            {
                return Ok(None);
            }
            Err(error) => return Err(Error::Io(error)),
        };
    }
    if !metadata.file_type().is_file() {
        return not_regular(links);
    }
    let too_large = || {
        Error::Limit(format!(
            "exclusion rule file exceeds {limit} bytes: {label}"
        ))
    };
    if metadata.len() > limit {
        return Err(too_large());
    }
    let mut flags = libc::O_NONBLOCK;
    if links != LinkPolicy::Follow {
        flags |= libc::O_NOFOLLOW;
    }
    let file = match fs::OpenOptions::new()
        .read(true)
        .custom_flags(flags)
        .open(path)
    {
        Ok(file) => file,
        // A symlink swapped in after the check.
        Err(error) if links == LinkPolicy::Skip && error.raw_os_error() == Some(libc::ELOOP) => {
            return Ok(None);
        }
        Err(error) => return Err(Error::Io(error)),
    };
    if !file.metadata()?.is_file() {
        return not_regular(links);
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut &file)
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(Some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Settings as if Git had reported `root/.git` as the common directory.
    fn with_git(root: &Path, global: Option<&Path>) -> Settings {
        Settings {
            git_common_dir: Some(root.join(".git")),
            global_excludes: global.map(Path::to_path_buf),
            case_insensitive: false,
        }
    }

    fn write(root: &Path, path: &str, text: &str) {
        let full = root.join(path);
        fs::create_dir_all(full.parent().unwrap()).unwrap();
        fs::write(full, text).unwrap();
    }

    fn select(selector: &Selector<'_>, path: &str, is_dir: bool) -> Selection {
        selector.classify(Path::new(path), is_dir, None).selection
    }

    fn excluded_by(selection: Selection) -> (String, String) {
        match selection {
            Selection::Excluded(rule) => (rule.source, rule.pattern),
            other => panic!("expected excluded, got {other:?}"),
        }
    }

    #[test]
    fn gitignore_patterns_follow_git_semantics() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".gitignore",
            ".env\ntarget/\n*.log\n!keep.log\n",
        );
        let tracked = Tracked::default();
        let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        assert!(selector.enter_directory(dir.path(), "").unwrap());
        assert_eq!(excluded_by(select(&selector, ".env", false)).1, ".env");
        assert_eq!(excluded_by(select(&selector, "a/b/.env", false)).1, ".env");
        // A directory-only rule does not match a file of that name.
        assert_eq!(select(&selector, "target", false), Selection::Included);
        assert_eq!(excluded_by(select(&selector, "target", true)).1, "target/");
        assert_eq!(excluded_by(select(&selector, "x.log", false)).1, "*.log");
        assert!(matches!(
            select(&selector, "keep.log", false),
            Selection::IncludedByRule(rule) if rule.pattern == "!keep.log"
        ));
        assert_eq!(select(&selector, "src/main.rs", false), Selection::Included);
    }

    #[test]
    fn deeper_gitignore_overrides_shallower() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "*.txt\n");
        write(dir.path(), "sub/.gitignore", "!keep.txt\n/only-here\n");
        let tracked = Tracked::default();
        let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        selector.enter_directory(dir.path(), "").unwrap();
        selector
            .enter_directory(&dir.path().join("sub"), "sub")
            .unwrap();
        assert!(matches!(
            select(&selector, "sub/keep.txt", false),
            Selection::IncludedByRule(_)
        ));
        assert_eq!(
            excluded_by(select(&selector, "sub/other.txt", false)),
            (".gitignore".to_owned(), "*.txt".to_owned())
        );
        // Anchored patterns are relative to the directory holding the file.
        assert_eq!(
            excluded_by(select(&selector, "sub/only-here", false)).0,
            "sub/.gitignore"
        );
        assert_eq!(
            select(&selector, "sub/deep/only-here", false),
            Selection::Included
        );
        selector.leave_directory();
        assert_eq!(excluded_by(select(&selector, "keep.txt", false)).1, "*.txt");
    }

    #[test]
    fn higher_sources_override_lower_ones() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global-ignore");
        fs::write(&global, "*.gen\n").unwrap();
        write(dir.path(), ".git/info/exclude", "info-only\n!*.gen\n");
        write(dir.path(), ".gitignore", "*.gen\n!pinned.gen\n");
        write(dir.path(), ".agentscmignore", "pinned.gen\nshared\n");
        write(
            dir.path(),
            ".agentscm-workspace/exclude",
            "!shared\nprivate\n",
        );
        let tracked = Tracked::default();
        let mut selector =
            Selector::open(dir.path(), &tracked, &with_git(dir.path(), Some(&global))).unwrap();
        selector.enter_directory(dir.path(), "").unwrap();
        assert_eq!(
            excluded_by(select(&selector, "pinned.gen", false)).0,
            ".agentscmignore"
        );
        assert!(matches!(
            select(&selector, "shared", false),
            Selection::IncludedByRule(rule) if rule.source == PRIVATE_EXCLUDE_PATH
        ));
        assert_eq!(
            excluded_by(select(&selector, "private", false)).0,
            PRIVATE_EXCLUDE_PATH
        );
        assert_eq!(
            excluded_by(select(&selector, "info-only", false)).0,
            INFO_EXCLUDE_SOURCE
        );
        // .gitignore beats info/exclude's whitelist.
        assert_eq!(
            excluded_by(select(&selector, "a.gen", false)).0,
            ".gitignore"
        );
    }

    #[test]
    fn git_sources_apply_only_with_a_git_common_dir() {
        let dir = tempfile::tempdir().unwrap();
        let global = dir.path().join("global-ignore");
        fs::write(&global, "from-global\n").unwrap();
        write(dir.path(), ".git/info/exclude", "from-info\n");
        let plain = tempfile::tempdir().unwrap();
        write(plain.path(), "info/exclude", "from-info\n");
        let tracked = Tracked::default();
        let with_git =
            Selector::open(dir.path(), &tracked, &with_git(dir.path(), Some(&global))).unwrap();
        assert_eq!(
            excluded_by(select(&with_git, "from-global", false)).0,
            GLOBAL_EXCLUDES_SOURCE
        );
        assert_eq!(
            excluded_by(select(&with_git, "from-info", false)).0,
            INFO_EXCLUDE_SOURCE
        );
        let without = Selector::open(plain.path(), &tracked, &Settings::default()).unwrap();
        assert_eq!(select(&without, "from-global", false), Selection::Included);
        assert_eq!(select(&without, "from-info", false), Selection::Included);
    }

    #[test]
    fn tracked_paths_are_never_excluded_and_children_inherit_exclusion() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "build/\n*.cfg\n");
        let tracked = Tracked::from_paths(["build/kept.txt", "a.cfg"]);
        let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        selector.enter_directory(dir.path(), "").unwrap();
        let build = selector.classify(Path::new("build"), true, None);
        assert_eq!(build.selection, Selection::Included);
        let inherited = build.child_exclusion.expect("children stay excluded");
        assert_eq!(
            selector
                .classify(Path::new("build/kept.txt"), false, Some(&inherited))
                .selection,
            Selection::Included
        );
        assert!(matches!(
            selector
                .classify(Path::new("build/new.txt"), false, Some(&inherited))
                .selection,
            Selection::Excluded(_)
        ));
        assert_eq!(select(&selector, "a.cfg", false), Selection::Included);
        assert!(matches!(
            select(&selector, "b.cfg", false),
            Selection::Excluded(_)
        ));
    }

    #[test]
    fn private_and_tracked_rule_files_refuse_links_and_oversize() {
        let dir = tempfile::tempdir().unwrap();
        let tracked = Tracked::default();
        fs::write(
            dir.path().join(".agentscmignore"),
            vec![b'a'; MAX_RULE_FILE_BYTES as usize + 1],
        )
        .unwrap();
        assert!(matches!(
            Selector::open(dir.path(), &tracked, &Settings::default()),
            Err(Error::Limit(message)) if message.contains(".agentscmignore")
        ));
        fs::remove_file(dir.path().join(".agentscmignore")).unwrap();
        fs::write(dir.path().join("real"), "x\n").unwrap();
        std::os::unix::fs::symlink("real", dir.path().join(".agentscmignore")).unwrap();
        assert!(matches!(
            Selector::open(dir.path(), &tracked, &Settings::default()),
            Err(Error::Invalid(_))
        ));
        fs::remove_file(dir.path().join(".agentscmignore")).unwrap();
        fs::create_dir_all(dir.path().join(".agentscm-workspace")).unwrap();
        std::os::unix::fs::symlink("../real", dir.path().join(".agentscm-workspace/exclude"))
            .unwrap();
        assert!(matches!(
            Selector::open(dir.path(), &tracked, &Settings::default()),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn non_utf8_rule_text_is_decoded_lossily() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".agentscmignore"), b"\xff\xfe\nplain\n").unwrap();
        let tracked = Tracked::default();
        let selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        assert!(matches!(
            select(&selector, "plain", false),
            Selection::Excluded(_)
        ));
    }

    #[test]
    fn in_tree_gitignore_links_are_not_rule_sources_and_global_links_are_followed() {
        let dir = tempfile::tempdir().unwrap();
        let tracked = Tracked::default();
        write(dir.path(), "real-rules", "hidden\n");
        std::os::unix::fs::symlink("real-rules", dir.path().join(".gitignore")).unwrap();
        let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        assert!(!selector.enter_directory(dir.path(), "").unwrap());
        assert_eq!(select(&selector, "hidden", false), Selection::Included);
        // A directory named .gitignore is not a rule source either.
        let other = tempfile::tempdir().unwrap();
        fs::create_dir(other.path().join(".gitignore")).unwrap();
        let mut selector = Selector::open(other.path(), &tracked, &Settings::default()).unwrap();
        assert!(!selector.enter_directory(other.path(), "").unwrap());

        // Global excludes and info/exclude may be symbolic links.
        let git = tempfile::tempdir().unwrap();
        fs::create_dir_all(git.path().join(".git/info")).unwrap();
        write(git.path(), "elsewhere/info-rules", "via-info\n");
        std::os::unix::fs::symlink(
            git.path().join("elsewhere/info-rules"),
            git.path().join(".git/info/exclude"),
        )
        .unwrap();
        write(git.path(), "elsewhere/global-rules", "via-global\n");
        std::os::unix::fs::symlink(
            git.path().join("elsewhere/global-rules"),
            git.path().join("global-link"),
        )
        .unwrap();
        let settings = with_git(git.path(), Some(&git.path().join("global-link")));
        let selector = Selector::open(git.path(), &tracked, &settings).unwrap();
        assert_eq!(
            excluded_by(select(&selector, "via-info", false)).0,
            INFO_EXCLUDE_SOURCE
        );
        assert_eq!(
            excluded_by(select(&selector, "via-global", false)).0,
            GLOBAL_EXCLUDES_SOURCE
        );
    }

    fn rules_of_size(lines: usize) -> String {
        // Long lines keep the byte cost high and the pattern count low.
        (0..lines)
            .map(|index| format!("{index:04}{}\n", "x".repeat(395)))
            .collect()
    }

    #[test]
    fn live_pattern_budget_counts_all_resident_sources() {
        let dir = tempfile::tempdir().unwrap();
        let tracked = Tracked::default();
        let lines: Vec<String> = (0..MAX_LIVE_RULE_PATTERNS + 1)
            .map(|index| format!("p{index}\n"))
            .collect();
        let (first, second) = lines.split_at(lines.len() / 2);
        write(dir.path(), ".agentscmignore", &first.concat());
        write(dir.path(), ".agentscm-workspace/exclude", &second.concat());
        assert!(matches!(
            Selector::open(dir.path(), &tracked, &Settings::default()),
            Err(Error::Limit(message)) if message.contains("pattern budget")
        ));
    }

    #[test]
    fn sibling_gitignore_files_are_credited_back_but_work_is_cumulative() {
        let dir = tempfile::tempdir().unwrap();
        let tracked = Tracked::default();
        // About 40 KiB and 100 patterns per file.
        write(dir.path(), "d/.gitignore", &rules_of_size(100));
        let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        // 150 siblings: 6 MiB of work in total, never more than one file resident.
        for _ in 0..150 {
            assert!(
                selector
                    .enter_directory(&dir.path().join("d"), "d")
                    .unwrap()
            );
            selector.leave_directory();
        }
        // The cumulative budget (8 MiB) is exhausted by further siblings.
        let result = (0..100).try_for_each(|_| {
            selector.enter_directory(&dir.path().join("d"), "d")?;
            selector.leave_directory();
            Ok::<(), Error>(())
        });
        assert!(matches!(result, Err(Error::Limit(message)) if message.contains("d/.gitignore")));
    }

    #[test]
    fn deeply_nested_gitignore_files_exceed_the_live_budget() {
        let dir = tempfile::tempdir().unwrap();
        let tracked = Tracked::default();
        let mut path = dir.path().to_path_buf();
        let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        let mut result = Ok(true);
        for depth in 0..40 {
            path = path.join("n");
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join(".gitignore"), rules_of_size(100)).unwrap();
            result = selector.enter_directory(&path, &format!("n{depth}"));
            if result.is_err() {
                break;
            }
        }
        // 1 MiB of live rules is reached before 40 files of about 40 KiB.
        assert!(matches!(result, Err(Error::Limit(message)) if message.contains(".gitignore")));
    }

    #[test]
    fn braces_are_literal_as_in_git() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".agentscmignore",
            "secret{a,b}\nbad{\nclose}\n\\{escaped\n",
        );
        let tracked = Tracked::default();
        let selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        assert_eq!(
            excluded_by(select(&selector, "secret{a,b}", false)).1,
            "secret{a,b}"
        );
        assert_eq!(select(&selector, "secreta", false), Selection::Included);
        assert!(matches!(
            select(&selector, "bad{", false),
            Selection::Excluded(_)
        ));
        assert!(matches!(
            select(&selector, "close}", false),
            Selection::Excluded(_)
        ));
        assert!(matches!(
            select(&selector, "{escaped", false),
            Selection::Excluded(_)
        ));
    }

    #[test]
    fn negations_follow_the_case_mode() {
        for (insensitive, expected_included) in [(false, false), (true, true)] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), ".agentscmignore", ".env\n!.ENV\n");
            let tracked = Tracked::default();
            let settings = Settings {
                git_common_dir: None,
                global_excludes: None,
                case_insensitive: insensitive,
            };
            let selector = Selector::open(dir.path(), &tracked, &settings).unwrap();
            let included = !matches!(select(&selector, ".env", false), Selection::Excluded(_));
            assert_eq!(included, expected_included, "insensitive={insensitive}");
            assert!(
                !matches!(select(&selector, ".ENV", false), Selection::Excluded(_)),
                "insensitive={insensitive}"
            );
        }
    }

    #[test]
    fn a_parent_exclusion_cannot_be_re_included_by_a_higher_source() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "build/\n");
        write(
            dir.path(),
            ".agentscm-workspace/exclude",
            "!build/keep.txt\n",
        );
        let tracked = Tracked::default();
        let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        selector.enter_directory(dir.path(), "").unwrap();
        let build = selector.classify(Path::new("build"), true, None);
        assert!(matches!(build.selection, Selection::Excluded(_)));
        // The walker never descends, and even a forced descent keeps the exclusion.
        let child = selector.classify(
            Path::new("build/keep.txt"),
            false,
            build.child_exclusion.as_ref(),
        );
        assert!(matches!(child.selection, Selection::Excluded(_)));
    }

    #[test]
    fn a_whitelist_with_a_trailing_tab_does_not_include_the_plain_name() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".agentscmignore", ".env\n!.env\t\n");
        let tracked = Tracked::default();
        let selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        assert!(matches!(
            select(&selector, ".env", false),
            Selection::Excluded(_)
        ));
        assert!(matches!(
            select(&selector, ".env\t", false),
            Selection::IncludedByRule(rule) if rule.pattern == "!.env\t"
        ));
    }

    #[test]
    fn suffix_rules_are_rejected_before_the_matching_program_runs() {
        let dir = tempfile::tempdir().unwrap();
        let rules: String = (0..6000).map(|n| format!("*.e{n}\n")).collect();
        write(dir.path(), ".agentscmignore", &rules);
        let tracked = Tracked::default();
        let selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        for index in 0..20_000 {
            let path = format!("d{}/file{index}.txt", index % 50);
            assert_eq!(select(&selector, &path, false), Selection::Included);
        }
        // Rejected rules are still charged, but far below the cap.
        assert!(selector.matching_work() > 0 && selector.matching_work() < MAX_MATCH_WORK / 2);
        selector.check_work().unwrap();
        // A path that does end in one of the suffixes still reaches the program and matches.
        assert!(matches!(
            select(&selector, "d/x.e5999", false),
            Selection::Excluded(rule) if rule.pattern == "*.e5999"
        ));
        assert!(selector.matching_work() > 0);
    }

    #[test]
    fn star_heavy_rules_trip_the_work_budget_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        let rules: String = (0..300).map(|_| "*a*a*a*a*a*a*a*a*a*a*b\n").collect();
        write(dir.path(), ".agentscmignore", &rules);
        let tracked = Tracked::default();
        let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        selector.set_work_limit(10_000_000);
        // Passes the suffix check but has too few `a` bytes to match, so every rule runs fully.
        let path = format!("{}{}b", "a".repeat(8), "x".repeat(190));
        let mut evaluated = 0;
        while selector.check_work().is_ok() && evaluated < 10_000 {
            select(&selector, &path, false);
            evaluated += 1;
        }
        // 300 rules x about 23 tokens x 201 positions is 1.4 million units per path.
        assert!(evaluated < 50, "{evaluated}");
        assert!(matches!(
            selector.check_work(),
            Err(Error::Limit(message))
                if message.contains(".agentscmignore") && message.contains("exclude its directory")
        ));
    }

    /// Total matching work for `rules` (written to `.agentscmignore`, with one inert private
    /// rule so the private file is evaluated first) over `paths`, with no cap.
    fn total_work(dir: &Path, paths: &[String]) -> u64 {
        let tracked = Tracked::default();
        let mut selector = Selector::open(dir, &tracked, &Settings::default()).unwrap();
        selector.set_work_limit(u64::MAX);
        for path in paths {
            select(&selector, path, false);
        }
        selector.matching_work()
    }

    #[test]
    fn crossing_the_cap_on_the_final_rule_of_the_final_path_still_trips_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".agentscm-workspace/exclude",
            "inert-private-rule\n",
        );
        let rules: String = (0..50)
            .map(|i| format!("{}b{}z\n", "?".repeat(i), "?".repeat(59 - i)))
            .collect();
        write(dir.path(), ".agentscmignore", &rules);
        // 61-byte names ending in `z` that no rule matches, so every rule runs fully.
        let paths: Vec<String> = (0..20).map(|n| format!("{n:0>60}z")).collect();
        let total = total_work(dir.path(), &paths);
        let tracked = Tracked::default();
        for (limit, trips) in [(total, false), (total - 1, true)] {
            let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
            selector.set_work_limit(limit);
            for path in &paths {
                select(&selector, path, false);
            }
            match (selector.check_work(), trips) {
                (Ok(()), false) => {}
                (Err(Error::Limit(message)), true) => {
                    assert!(message.contains(".agentscmignore"), "{message}");
                    assert!(message.contains("exclude its directory"), "{message}");
                }
                (outcome, _) => panic!("limit {limit}: {outcome:?}"),
            }
        }
    }

    #[test]
    fn pre_check_only_workloads_are_charged_and_trip_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".agentscm-workspace/exclude",
            "unrelated-name\n",
        );
        let rules: String = (0..3000).map(|n| format!("*.e{n}\n")).collect();
        write(dir.path(), ".agentscmignore", &rules);
        let tracked = Tracked::default();
        // `*` in the private file would match, but the heavy file is consulted only when the
        // private file does not match; make it not match with a negation-free name check.
        fs::write(
            dir.path().join(".agentscm-workspace/exclude"),
            "unrelated-name\n",
        )
        .unwrap();
        let mut selector = Selector::open(dir.path(), &tracked, &Settings::default()).unwrap();
        selector.set_work_limit(1_000_000);
        let mut visited = 0;
        while selector.check_work().is_ok() && visited < 1000 {
            select(&selector, &format!("dir/{}.txt", "x".repeat(200)), false);
            visited += 1;
        }
        // No evaluation ran the matching program: every rule fails the suffix check, yet the
        // work still crossed the cap within a few paths.
        assert!(visited < 100, "{visited}");
        assert!(matches!(
            selector.check_work(),
            Err(Error::Limit(message)) if message.contains(".agentscmignore")
        ));
    }
}
