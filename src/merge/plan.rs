use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{ConflictKind, ConflictPath, MergeConflict, text};
use crate::error::{Error, Result};
use crate::store::{Engine, MAX_FILE_PAGE, SourceRead};

const MAX_MANIFEST_FILES: usize = 100_000;
const MAX_UNION_FILES: usize = 100_000;
const MAX_MANIFEST_PAGES: usize = 100_000;
const MAX_PLAN_PATHS: usize = 4_096;
const MAX_PLAN_WORK: usize = 4_000_000;
const MAX_PREFIX_WORK: usize = 1_000_000;
const MAX_RETAINED_PREFIXES: usize = 100_000;
const MAX_RETAINED_PREVIEW_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PlannedFile {
    pub(crate) path: String,
    pub(crate) base: Option<String>,
    pub(crate) left: Option<String>,
    pub(crate) right: Option<String>,
    pub(crate) result: Option<String>,
    pub(crate) conflict: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub(crate) files: Vec<PlannedFile>,
    pub(crate) conflicts: Vec<MergeConflict>,
}

struct Work {
    remaining: usize,
    prefix_remaining: usize,
}

impl Work {
    fn new() -> Self {
        Self {
            remaining: MAX_PLAN_WORK,
            prefix_remaining: MAX_PREFIX_WORK,
        }
    }

    fn charge(&mut self, amount: usize, activity: &str) -> Result<()> {
        self.remaining = self.remaining.checked_sub(amount).ok_or_else(|| {
            Error::Limit(format!(
                "merge planning exceeded its {activity} work budget"
            ))
        })?;
        Ok(())
    }

    fn charge_prefix(&mut self) -> Result<()> {
        self.charge(1, "total")?;
        self.prefix_remaining = self.prefix_remaining.checked_sub(1).ok_or_else(|| {
            Error::Limit(format!(
                "merge planning examined more than {MAX_PREFIX_WORK} directory prefixes"
            ))
        })?;
        Ok(())
    }
}

#[derive(Clone)]
struct Issue {
    kind: ConflictKind,
    paths: BTreeSet<String>,
    preview: Option<Vec<u8>>,
    preview_truncated: bool,
}

pub(crate) fn build<R: SourceRead + ?Sized>(
    source: &R,
    engine: &Engine,
    base: Option<&str>,
    left: &str,
    right: &str,
) -> Result<Plan> {
    let mut work = Work::new();
    let base_files = match base {
        Some(revision) => load_manifest(source, revision, &mut work)?,
        None => BTreeMap::new(),
    };
    let left_files = load_manifest(source, left, &mut work)?;
    let right_files = load_manifest(source, right, &mut work)?;

    let mut paths = BTreeSet::new();
    paths.extend(base_files.keys().cloned());
    paths.extend(left_files.keys().cloned());
    paths.extend(right_files.keys().cloned());
    if paths.len() > MAX_UNION_FILES {
        return Err(Error::Limit(format!(
            "merge has more than {MAX_UNION_FILES} union file paths"
        )));
    }

    let mut result_changes = BTreeMap::<String, Option<String>>::new();
    let mut issues = Vec::new();
    let mut issue_paths = BTreeSet::new();
    let mut text_budget = text::TextBudget::default();
    let mut preview_remaining = MAX_RETAINED_PREVIEW_BYTES;

    for path in &paths {
        work.charge(1, "file-decision")?;
        let base_blob = base_files.get(path);
        let left_blob = left_files.get(path);
        let right_blob = right_files.get(path);

        let result = if left_blob == right_blob {
            left_blob.cloned()
        } else if left_blob == base_blob {
            right_blob.cloned()
        } else if right_blob == base_blob {
            left_blob.cloned()
        } else {
            match (base_blob, left_blob, right_blob) {
                (None, Some(_), Some(_)) => {
                    add_issue(
                        &mut issues,
                        &mut issue_paths,
                        ConflictKind::AddAdd,
                        [path.clone()],
                        None,
                        false,
                    )?;
                    left_blob.cloned()
                }
                (Some(_), None, Some(_)) | (Some(_), Some(_), None) => {
                    add_issue(
                        &mut issues,
                        &mut issue_paths,
                        ConflictKind::EditDelete,
                        [path.clone()],
                        None,
                        false,
                    )?;
                    left_blob.cloned()
                }
                (Some(_), Some(_), Some(_)) => {
                    let base_revision = base.expect("a base blob requires a base revision");
                    let base_bytes = source.read_file(base_revision, path)?;
                    let left_bytes = source.read_file(left, path)?;
                    let right_bytes = source.read_file(right, path)?;
                    match text::merge(&base_bytes, &left_bytes, &right_bytes, &mut text_budget) {
                        text::TextOutcome::Merged(bytes) => Some(engine.publish_blob(&bytes)?),
                        text::TextOutcome::Conflict { preview, truncated } => {
                            let (preview, truncated) =
                                retain_preview(preview, truncated, &mut preview_remaining);
                            add_issue(
                                &mut issues,
                                &mut issue_paths,
                                ConflictKind::Content,
                                [path.clone()],
                                preview,
                                truncated,
                            )?;
                            left_blob.cloned()
                        }
                        text::TextOutcome::Unsupported(reason) => {
                            let kind = match reason {
                                text::TextReason::Binary | text::TextReason::InvalidUtf8 => {
                                    ConflictKind::Binary
                                }
                                _ => ConflictKind::TextProfile,
                            };
                            add_issue(
                                &mut issues,
                                &mut issue_paths,
                                kind,
                                [path.clone()],
                                None,
                                false,
                            )?;
                            left_blob.cloned()
                        }
                    }
                }
                _ => {
                    return Err(Error::Invalid(
                        "merge file decision reached an inconsistent state".into(),
                    ));
                }
            }
        };

        if result.as_ref() != left_blob {
            result_changes.insert(path.clone(), result);
        }
        if result_changes.len() + issue_paths.len() > MAX_PLAN_PATHS {
            return Err(Error::Limit(format!(
                "merge has more than {MAX_PLAN_PATHS} changed or grouped paths"
            )));
        }
    }

    detect_rename_conflicts(
        &base_files,
        &left_files,
        &right_files,
        &mut issues,
        &mut issue_paths,
        &mut work,
    )?;
    validate_plan_path_count(&result_changes, &issue_paths)?;

    let mut candidate = left_files.clone();
    for (path, result) in &result_changes {
        if issue_paths.contains(path) {
            continue;
        }
        match result {
            Some(blob) => {
                candidate.insert(path.clone(), blob.clone());
            }
            None => {
                candidate.remove(path);
            }
        }
    }
    detect_structural_conflicts(&candidate, &mut issues, &mut issue_paths, &mut work)?;
    validate_plan_path_count(&result_changes, &issue_paths)?;

    finish_plan(
        base,
        left,
        right,
        [&base_files, &left_files, &right_files],
        result_changes,
        issues,
    )
}

pub(crate) fn validate_results<R: SourceRead + ?Sized>(
    source: &R,
    left: &str,
    files: &[PlannedFile],
) -> Result<()> {
    if files.len() > MAX_PLAN_PATHS {
        return Err(Error::Limit(format!(
            "merge has more than {MAX_PLAN_PATHS} changed or grouped paths"
        )));
    }

    let mut work = Work::new();
    let mut candidate = load_manifest(source, left, &mut work)?;
    let mut previous = None;
    for file in files {
        work.charge(1, "resolved-file")?;
        if previous.is_some_and(|path: &str| path >= file.path.as_str()) {
            return Err(Error::Invalid(
                "planned merge files are not in unique path order".into(),
            ));
        }
        previous = Some(file.path.as_str());
        if candidate.get(&file.path) != file.left.as_ref() {
            return Err(Error::Invalid(format!(
                "planned left blob does not match revision at path: {}",
                file.path
            )));
        }
        match &file.result {
            Some(blob) => {
                candidate.insert(file.path.clone(), blob.clone());
            }
            None => {
                candidate.remove(&file.path);
            }
        }
    }
    if candidate.len() > MAX_UNION_FILES {
        return Err(Error::Limit(format!(
            "resolved merge has more than {MAX_UNION_FILES} files"
        )));
    }

    let mut issues = Vec::new();
    let mut issue_paths = BTreeSet::new();
    detect_structural_conflicts(&candidate, &mut issues, &mut issue_paths, &mut work)?;
    if !issues.is_empty() {
        return Err(Error::Conflict(format!(
            "resolved merge results create {} structural collision groups",
            issues.len()
        )));
    }
    Ok(())
}

fn validate_plan_path_count(
    result_changes: &BTreeMap<String, Option<String>>,
    issue_paths: &BTreeSet<String>,
) -> Result<()> {
    let mut retained: BTreeSet<&str> = result_changes.keys().map(String::as_str).collect();
    retained.extend(issue_paths.iter().map(String::as_str));
    if retained.len() > MAX_PLAN_PATHS {
        return Err(Error::Limit(format!(
            "merge has more than {MAX_PLAN_PATHS} changed or grouped paths"
        )));
    }
    Ok(())
}

fn load_manifest<R: SourceRead + ?Sized>(
    source: &R,
    revision: &str,
    work: &mut Work,
) -> Result<BTreeMap<String, String>> {
    let mut files = BTreeMap::new();
    let mut cursor = None;
    let mut pages = 0usize;
    loop {
        pages += 1;
        if pages > MAX_MANIFEST_PAGES {
            return Err(Error::Limit(format!(
                "manifest requires more than {MAX_MANIFEST_PAGES} pages"
            )));
        }
        work.charge(1, "manifest-page")?;
        let page = source.list_files(revision, cursor.as_deref(), MAX_FILE_PAGE)?;
        work.charge(page.entries.len(), "manifest-entry")?;
        for entry in page.entries {
            // Blob ids alone decide merges, so a kind could be dropped silently.
            crate::store::require_regular_entry("merge", &entry.path, entry.kind)?;
            if files.insert(entry.path.clone(), entry.blob).is_some() {
                return Err(Error::Invalid(format!(
                    "manifest page repeated path: {}",
                    entry.path
                )));
            }
            if files.len() > MAX_MANIFEST_FILES {
                return Err(Error::Limit(format!(
                    "manifest has more than {MAX_MANIFEST_FILES} files"
                )));
            }
        }
        match page.next {
            Some(next) => {
                if cursor.as_deref() == Some(next.as_str()) {
                    return Err(Error::Invalid("manifest cursor did not advance".into()));
                }
                cursor = Some(next);
            }
            None => break,
        }
    }
    Ok(files)
}

fn retain_preview(
    mut preview: Vec<u8>,
    already_truncated: bool,
    remaining: &mut usize,
) -> (Option<Vec<u8>>, bool) {
    let truncated = already_truncated || preview.len() > *remaining;
    preview.truncate(*remaining);
    *remaining -= preview.len();
    if preview.is_empty() && truncated {
        (None, true)
    } else {
        (Some(preview), truncated)
    }
}

fn add_issue<I>(
    issues: &mut Vec<Issue>,
    issue_paths: &mut BTreeSet<String>,
    kind: ConflictKind,
    paths: I,
    preview: Option<Vec<u8>>,
    preview_truncated: bool,
) -> Result<()>
where
    I: IntoIterator<Item = String>,
{
    let paths: BTreeSet<String> = paths.into_iter().collect();
    if paths.is_empty() {
        return Err(Error::Invalid("merge conflict has no paths".into()));
    }
    issue_paths.extend(paths.iter().cloned());
    if issue_paths.len() > MAX_PLAN_PATHS {
        return Err(Error::Limit(format!(
            "merge has more than {MAX_PLAN_PATHS} conflict paths"
        )));
    }
    issues.push(Issue {
        kind,
        paths,
        preview,
        preview_truncated,
    });
    Ok(())
}

fn paths_by_blob<'a>(
    files: &'a BTreeMap<String, String>,
    work: &mut Work,
) -> Result<BTreeMap<&'a str, Vec<&'a str>>> {
    let mut paths = BTreeMap::<&str, Vec<&str>>::new();
    for (path, blob) in files {
        work.charge(1, "rename-index")?;
        paths.entry(blob).or_default().push(path);
    }
    Ok(paths)
}

fn detect_rename_conflicts(
    base: &BTreeMap<String, String>,
    left: &BTreeMap<String, String>,
    right: &BTreeMap<String, String>,
    issues: &mut Vec<Issue>,
    issue_paths: &mut BTreeSet<String>,
    work: &mut Work,
) -> Result<()> {
    let base_by_blob = paths_by_blob(base, work)?;
    let left_by_blob = paths_by_blob(left, work)?;
    let right_by_blob = paths_by_blob(right, work)?;
    for (blob, base_paths) in &base_by_blob {
        work.charge(1, "rename-classification")?;
        let left_paths = left_by_blob.get(blob).map(Vec::as_slice).unwrap_or(&[]);
        let right_paths = right_by_blob.get(blob).map(Vec::as_slice).unwrap_or(&[]);
        let left_moves = has_identity_movement(base_paths, left_paths);
        let right_moves = has_identity_movement(base_paths, right_paths);
        if !left_moves && !right_moves {
            continue;
        }

        let mut conflict_paths = BTreeSet::new();
        if base_paths.len() != 1 || left_paths.len() > 1 || right_paths.len() > 1 {
            extend_paths(&mut conflict_paths, base_paths);
            extend_paths(&mut conflict_paths, left_paths);
            extend_paths(&mut conflict_paths, right_paths);
        } else {
            let old = base_paths[0];
            if left_moves {
                let target = left_paths[0];
                if !pure_rename(base, left, old, target, blob)
                    || !rename_compatible(right, old, target, blob)
                {
                    conflict_paths.insert(old.to_owned());
                    conflict_paths.insert(target.to_owned());
                    extend_paths(&mut conflict_paths, right_paths);
                }
            }
            if right_moves {
                let target = right_paths[0];
                if !pure_rename(base, right, old, target, blob)
                    || !rename_compatible(left, old, target, blob)
                {
                    conflict_paths.insert(old.to_owned());
                    conflict_paths.insert(target.to_owned());
                    extend_paths(&mut conflict_paths, left_paths);
                }
            }
        }
        if !conflict_paths.is_empty() {
            add_issue(
                issues,
                issue_paths,
                ConflictKind::Rename,
                conflict_paths,
                None,
                false,
            )?;
        }
    }
    Ok(())
}

fn has_identity_movement(base: &[&str], side: &[&str]) -> bool {
    base.iter().any(|path| side.binary_search(path).is_err())
        && side.iter().any(|path| base.binary_search(path).is_err())
}

fn pure_rename(
    base: &BTreeMap<String, String>,
    side: &BTreeMap<String, String>,
    old: &str,
    target: &str,
    blob: &str,
) -> bool {
    old != target
        && base.get(target).is_none()
        && side.get(old).is_none()
        && side.get(target).is_some_and(|value| value == blob)
}

fn rename_compatible(
    opposite: &BTreeMap<String, String>,
    old: &str,
    target: &str,
    blob: &str,
) -> bool {
    (opposite.get(old).is_some_and(|value| value == blob) && opposite.get(target).is_none())
        || (opposite.get(old).is_none() && opposite.get(target).is_some_and(|value| value == blob))
}

fn extend_paths(target: &mut BTreeSet<String>, paths: &[&str]) {
    target.extend(paths.iter().map(|path| (*path).to_owned()));
}

fn detect_structural_conflicts(
    candidate: &BTreeMap<String, String>,
    issues: &mut Vec<Issue>,
    issue_paths: &mut BTreeSet<String>,
    work: &mut Work,
) -> Result<()> {
    let mut portable_files = BTreeMap::<String, String>::new();
    for path in candidate.keys() {
        work.charge(1, "portable-file")?;
        let folded = path.to_ascii_lowercase();
        if let Some(existing) = portable_files.insert(folded, path.clone())
            && existing != *path
        {
            add_issue(
                issues,
                issue_paths,
                ConflictKind::Portable,
                [existing, path.clone()],
                None,
                false,
            )?;
        }
    }

    let mut portable_directories = BTreeMap::<String, String>::new();
    let mut colliding_directories = BTreeSet::new();
    for path in candidate.keys() {
        work.charge(1, "structural-file")?;
        for prefix in directory_prefixes(path) {
            work.charge_prefix()?;
            if candidate.contains_key(prefix) {
                add_issue(
                    issues,
                    issue_paths,
                    ConflictKind::FileDirectory,
                    [prefix.to_owned(), path.clone()],
                    None,
                    false,
                )?;
            }
            let folded = prefix.to_ascii_lowercase();
            // An exact file/directory collision was already recorded above.
            // Only differently spelled portable matches add a separate issue.
            if let Some(file) = portable_files
                .get(&folded)
                .filter(|file| file.as_str() != prefix)
            {
                add_issue(
                    issues,
                    issue_paths,
                    ConflictKind::Portable,
                    [file.clone(), path.clone()],
                    None,
                    false,
                )?;
            }
            match portable_directories.entry(folded.clone()) {
                std::collections::btree_map::Entry::Vacant(entry) => {
                    entry.insert(prefix.to_owned());
                    if portable_directories.len() > MAX_RETAINED_PREFIXES {
                        return Err(Error::Limit(format!(
                            "merge has more than {MAX_RETAINED_PREFIXES} distinct directory prefixes"
                        )));
                    }
                }
                std::collections::btree_map::Entry::Occupied(entry) => {
                    if entry.get() != prefix {
                        colliding_directories.insert(folded);
                    }
                }
            }
        }
    }

    if !colliding_directories.is_empty() {
        let candidate_paths: Vec<&String> = candidate.keys().collect();
        let mut union = UnionFind::new(candidate_paths.len());
        let mut witnesses = BTreeMap::<&str, usize>::new();
        let mut involved = BTreeSet::<usize>::new();
        let mut new_issue_paths = 0usize;
        for (index, path) in candidate_paths.iter().enumerate() {
            work.charge(1, "portable-directory-file")?;
            for prefix in directory_prefixes(path) {
                work.charge_prefix()?;
                let folded = prefix.to_ascii_lowercase();
                let Some(collision) = colliding_directories.get(&folded) else {
                    continue;
                };
                if involved.insert(index) && !issue_paths.contains(path.as_str()) {
                    new_issue_paths += 1;
                    if issue_paths.len() + new_issue_paths > MAX_PLAN_PATHS {
                        return Err(Error::Limit(format!(
                            "merge has more than {MAX_PLAN_PATHS} conflict paths"
                        )));
                    }
                }
                match witnesses.entry(collision.as_str()) {
                    std::collections::btree_map::Entry::Vacant(entry) => {
                        entry.insert(index);
                    }
                    std::collections::btree_map::Entry::Occupied(entry) => {
                        union.join(*entry.get(), index);
                    }
                }
            }
        }

        let mut components = BTreeMap::<usize, Vec<usize>>::new();
        for index in involved {
            components.entry(union.root(index)).or_default().push(index);
        }
        for indexes in components.into_values() {
            add_issue(
                issues,
                issue_paths,
                ConflictKind::Portable,
                indexes
                    .into_iter()
                    .map(|index| candidate_paths[index].clone()),
                None,
                false,
            )?;
        }
    }
    Ok(())
}

fn directory_prefixes(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices('/').map(|(index, _)| &path[..index])
}

fn finish_plan(
    base_revision: Option<&str>,
    left_revision: &str,
    right_revision: &str,
    manifests: [&BTreeMap<String, String>; 3],
    mut result_changes: BTreeMap<String, Option<String>>,
    issues: Vec<Issue>,
) -> Result<Plan> {
    let [base, left, right] = manifests;
    let mut path_indexes = BTreeMap::<String, usize>::new();
    for issue in &issues {
        for path in &issue.paths {
            if !path_indexes.contains_key(path) {
                let index = path_indexes.len();
                path_indexes.insert(path.clone(), index);
            }
        }
    }
    if path_indexes.len() > MAX_PLAN_PATHS {
        return Err(Error::Limit(format!(
            "merge has more than {MAX_PLAN_PATHS} conflict paths"
        )));
    }

    let mut union = UnionFind::new(path_indexes.len());
    for issue in &issues {
        let mut indexes = issue.paths.iter().map(|path| path_indexes[path]);
        if let Some(first) = indexes.next() {
            for index in indexes {
                union.join(first, index);
            }
        }
    }

    #[derive(Default)]
    struct Component {
        paths: BTreeSet<String>,
        issue_indexes: Vec<usize>,
    }
    let mut components = BTreeMap::<usize, Component>::new();
    for (path, index) in &path_indexes {
        components
            .entry(union.root(*index))
            .or_default()
            .paths
            .insert(path.clone());
    }
    for (issue_index, issue) in issues.iter().enumerate() {
        let first = issue
            .paths
            .first()
            .expect("issues are validated as non-empty");
        components
            .entry(union.root(path_indexes[first]))
            .or_default()
            .issue_indexes
            .push(issue_index);
    }

    let mut components: Vec<Component> = components.into_values().collect();
    components.sort_by(|a, b| a.paths.first().cmp(&b.paths.first()));
    let mut conflicts = Vec::with_capacity(components.len());
    let mut conflict_by_path = BTreeMap::<String, String>::new();
    for component in components {
        let kind = component
            .issue_indexes
            .iter()
            .map(|index| issues[*index].kind)
            .min_by_key(|kind| conflict_kind_priority(*kind))
            .expect("a conflict component has an issue");
        let issue = if component.issue_indexes.len() == 1 {
            Some(&issues[component.issue_indexes[0]])
        } else {
            None
        };
        let had_preview = component
            .issue_indexes
            .iter()
            .any(|index| issues[*index].preview.is_some() || issues[*index].preview_truncated);
        let preview = issue.and_then(|issue| issue.preview.clone());
        let preview_truncated = issue
            .map(|issue| issue.preview_truncated)
            .unwrap_or(had_preview);
        let conflict_paths: Vec<ConflictPath> = component
            .paths
            .iter()
            .map(|path| ConflictPath {
                path: path.clone(),
                base: base.get(path).cloned(),
                left: left.get(path).cloned(),
                right: right.get(path).cloned(),
            })
            .collect();
        let id = conflict_id(
            base_revision,
            left_revision,
            right_revision,
            kind,
            &conflict_paths,
        );
        for path in &component.paths {
            result_changes.remove(path);
            conflict_by_path.insert(path.clone(), id.clone());
        }
        conflicts.push(MergeConflict {
            resolution: None,
            id,
            kind,
            paths: conflict_paths,
            resolved: false,
            preview,
            preview_truncated,
        });
    }

    let mut planned_paths: BTreeSet<String> = result_changes.keys().cloned().collect();
    planned_paths.extend(conflict_by_path.keys().cloned());
    if planned_paths.len() > MAX_PLAN_PATHS {
        return Err(Error::Limit(format!(
            "merge has more than {MAX_PLAN_PATHS} changed or grouped paths"
        )));
    }
    let files = planned_paths
        .into_iter()
        .map(|path| {
            let conflict = conflict_by_path.get(&path).cloned();
            let result = if conflict.is_some() {
                left.get(&path).cloned()
            } else {
                result_changes
                    .get(&path)
                    .expect("planned automatic path has a result")
                    .clone()
            };
            PlannedFile {
                base: base.get(&path).cloned(),
                left: left.get(&path).cloned(),
                right: right.get(&path).cloned(),
                path,
                result,
                conflict,
            }
        })
        .collect();
    Ok(Plan { files, conflicts })
}

fn conflict_kind_priority(kind: ConflictKind) -> u8 {
    match kind {
        ConflictKind::Portable => 0,
        ConflictKind::FileDirectory => 1,
        ConflictKind::Rename => 2,
        ConflictKind::EditDelete => 3,
        ConflictKind::AddAdd => 4,
        ConflictKind::Binary => 5,
        ConflictKind::TextProfile => 6,
        ConflictKind::Content => 7,
    }
}

fn conflict_kind_name(kind: ConflictKind) -> &'static [u8] {
    match kind {
        ConflictKind::Content => b"content",
        ConflictKind::Binary => b"binary",
        ConflictKind::TextProfile => b"text_profile",
        ConflictKind::EditDelete => b"edit_delete",
        ConflictKind::AddAdd => b"add_add",
        ConflictKind::Rename => b"rename",
        ConflictKind::FileDirectory => b"file_directory",
        ConflictKind::Portable => b"portable",
    }
}

fn conflict_id(
    base: Option<&str>,
    left: &str,
    right: &str,
    kind: ConflictKind,
    paths: &[ConflictPath],
) -> String {
    let mut digest = Sha256::new();
    digest.update(b"AGSCM-MERGE-CONFLICT-V1\0");
    hash_optional(&mut digest, base);
    hash_bytes(&mut digest, left.as_bytes());
    hash_bytes(&mut digest, right.as_bytes());
    hash_bytes(&mut digest, conflict_kind_name(kind));
    digest.update((paths.len() as u64).to_be_bytes());
    for path in paths {
        hash_bytes(&mut digest, path.path.as_bytes());
        hash_optional(&mut digest, path.base.as_deref());
        hash_optional(&mut digest, path.left.as_deref());
        hash_optional(&mut digest, path.right.as_deref());
    }
    let bytes = digest.finalize();
    let mut encoded = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn hash_optional(digest: &mut Sha256, value: Option<&str>) {
    match value {
        Some(value) => {
            digest.update([1]);
            hash_bytes(digest, value.as_bytes());
        }
        None => digest.update([0]),
    }
}

fn hash_bytes(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

struct UnionFind {
    parents: Vec<usize>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        Self {
            parents: (0..len).collect(),
        }
    }

    fn root(&mut self, mut index: usize) -> usize {
        while self.parents[index] != index {
            let parent = self.parents[index];
            self.parents[index] = self.parents[parent];
            index = self.parents[index];
        }
        index
    }

    fn join(&mut self, left: usize, right: usize) {
        let left = self.root(left);
        let right = self.root(right);
        if left != right {
            let (root, child) = if left < right {
                (left, right)
            } else {
                (right, left)
            };
            self.parents[child] = root;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DiffResult, FileChange, Revision};
    use crate::store::{BlobChange, DiffPage, FilePage};

    fn checkpoint(
        engine: &Engine,
        parent: Option<&str>,
        changes: &[(&str, Option<&[u8]>)],
    ) -> String {
        let changes: Vec<FileChange> = changes
            .iter()
            .map(|(path, content)| FileChange {
                path: (*path).to_owned(),
                content: content.as_ref().map(|bytes| (*bytes).to_vec()),
            })
            .collect();
        engine.checkpoint(parent, &changes).expect("checkpoint")
    }

    fn apply_plan(engine: &Engine, left: &str, right: &str, plan: &Plan) -> String {
        let changes: Vec<BlobChange> = plan
            .files
            .iter()
            .filter(|file| file.conflict.is_none() && file.result != file.left)
            .map(|file| BlobChange {
                path: file.path.clone(),
                blob: file.result.clone(),
            })
            .collect();
        engine
            .checkpoint_with_blob_changes(&[left.to_owned(), right.to_owned()], &changes)
            .expect("merged revision")
    }

    fn kinds(plan: &Plan) -> Vec<ConflictKind> {
        plan.conflicts
            .iter()
            .map(|conflict| conflict.kind)
            .collect()
    }

    fn resolved_add(path: &str, blob: &str) -> PlannedFile {
        PlannedFile {
            path: path.to_owned(),
            base: None,
            left: None,
            right: Some(blob.to_owned()),
            result: Some(blob.to_owned()),
            conflict: Some("resolved-group".into()),
        }
    }

    #[test]
    fn merges_nonoverlapping_text_and_publishes_the_result() {
        let directory = tempfile::tempdir().expect("tempdir");
        let engine = Engine::open(directory.path()).expect("engine");
        let base = checkpoint(&engine, None, &[("file", Some(b"alpha\nmiddle\nomega\n"))]);
        let left = checkpoint(
            &engine,
            Some(&base),
            &[("file", Some(b"ALPHA\nmiddle\nomega\n"))],
        );
        let right = checkpoint(
            &engine,
            Some(&base),
            &[("file", Some(b"alpha\nmiddle\nOMEGA\n"))],
        );

        let plan = build(&engine, &engine, Some(&base), &left, &right).expect("plan");
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.files.len(), 1);
        let revision = apply_plan(&engine, &left, &right, &plan);
        assert_eq!(
            engine.read_file(&revision, "file").expect("merged file"),
            b"ALPHA\nmiddle\nOMEGA\n"
        );
    }

    #[test]
    fn records_content_binary_edit_delete_and_add_add_conflicts() {
        let directory = tempfile::tempdir().expect("tempdir");
        let engine = Engine::open(directory.path()).expect("engine");
        let base = checkpoint(
            &engine,
            None,
            &[
                ("content", Some(b"base\n")),
                ("binary", Some(&[0, 1])),
                ("deleted", Some(b"base\n")),
            ],
        );
        let left = checkpoint(
            &engine,
            Some(&base),
            &[
                ("content", Some(b"left\n")),
                ("binary", Some(&[0, 2])),
                ("deleted", None),
                ("added", Some(b"left\n")),
            ],
        );
        let right = checkpoint(
            &engine,
            Some(&base),
            &[
                ("content", Some(b"right\n")),
                ("binary", Some(&[0, 3])),
                ("deleted", Some(b"right\n")),
                ("added", Some(b"right\n")),
            ],
        );

        let plan = build(&engine, &engine, Some(&base), &left, &right).expect("plan");
        assert_eq!(
            kinds(&plan),
            vec![
                ConflictKind::AddAdd,
                ConflictKind::Binary,
                ConflictKind::Content,
                ConflictKind::EditDelete,
            ]
        );
        assert_eq!(plan.files.len(), 4);
        assert!(plan.files.iter().all(|file| file.result == file.left));
        let content = plan
            .conflicts
            .iter()
            .find(|conflict| conflict.kind == ConflictKind::Content)
            .expect("content conflict");
        assert!(content.preview.is_some());
    }

    #[test]
    fn groups_file_directory_and_portable_directory_collisions() {
        let directory = tempfile::tempdir().expect("tempdir");
        let engine = Engine::open(directory.path()).expect("engine");
        let base = checkpoint(&engine, None, &[]);

        let left = checkpoint(
            &engine,
            Some(&base),
            &[("node", Some(b"left")), ("Dir/a", Some(b"a"))],
        );
        let right = checkpoint(
            &engine,
            Some(&base),
            &[("node/child", Some(b"right")), ("dir/b", Some(b"b"))],
        );
        let plan = build(&engine, &engine, Some(&base), &left, &right).expect("plan");

        assert_eq!(plan.conflicts.len(), 2);
        assert!(
            plan.conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::FileDirectory)
        );
        assert!(
            plan.conflicts
                .iter()
                .any(|conflict| conflict.kind == ConflictKind::Portable)
        );
        let mut seen = BTreeSet::new();
        for conflict in &plan.conflicts {
            for path in &conflict.paths {
                assert!(seen.insert(path.path.clone()));
            }
        }
    }

    #[test]
    fn deep_portable_prefix_collisions_retain_one_bounded_component() {
        let directory = tempfile::tempdir().expect("tempdir");
        let engine = Engine::open(directory.path()).expect("engine");
        let base = checkpoint(&engine, None, &[]);
        let upper = std::iter::repeat_n("D", 128).collect::<Vec<_>>().join("/");
        let lower = std::iter::repeat_n("d", 128).collect::<Vec<_>>().join("/");
        // Validate representative side shapes with the actual native writer.
        // The large collision test exercises structural metadata directly:
        // materializing 1000 deep files would exceed the per-checkpoint prefix
        // budget and create excessive intermediate objects for this unit test.
        let left_path = format!("{upper}/left");
        let right_path = format!("{lower}/right");
        let left = checkpoint(&engine, Some(&base), &[(left_path.as_str(), Some(b"left"))]);
        let right = checkpoint(
            &engine,
            Some(&base),
            &[(right_path.as_str(), Some(b"right"))],
        );
        let left_blob = engine.list_files(&left, None, 1).unwrap().entries[0]
            .blob
            .clone();
        let right_blob = engine.list_files(&right, None, 1).unwrap().entries[0]
            .blob
            .clone();
        let mut candidate = BTreeMap::new();
        for index in 0..500 {
            candidate.insert(format!("{upper}/left-{index:04}"), left_blob.clone());
            candidate.insert(format!("{lower}/right-{index:04}"), right_blob.clone());
        }
        let mut issues = Vec::new();
        let mut issue_paths = BTreeSet::new();
        detect_structural_conflicts(&candidate, &mut issues, &mut issue_paths, &mut Work::new())
            .unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, ConflictKind::Portable);
        assert_eq!(issue_paths.len(), 1000);
        assert_eq!(issues[0].paths.len(), 1000);
        let retained_group_paths: usize = issues.iter().map(|issue| issue.paths.len()).sum();
        assert_eq!(retained_group_paths, 1000);
        assert!(retained_group_paths <= MAX_PLAN_PATHS);
    }

    #[test]
    fn combined_manual_choices_reject_cross_group_structural_collisions() {
        let directory = tempfile::tempdir().expect("tempdir");
        let engine = Engine::open(directory.path()).expect("engine");
        let left = checkpoint(&engine, None, &[]);
        let blob = engine.publish_blob(b"chosen").expect("blob");

        for paths in [
            ["node", "node/child"],
            ["Dir", "dir/child"],
            ["Dir/a", "dir/b"],
        ] {
            let first = resolved_add(paths[0], &blob);
            let second = resolved_add(paths[1], &blob);
            validate_results(&engine, &left, std::slice::from_ref(&first))
                .expect("first choice is valid alone");
            validate_results(&engine, &left, std::slice::from_ref(&second))
                .expect("second choice is valid alone");

            let mut combined = vec![first, second];
            combined.sort_by(|a, b| a.path.cmp(&b.path));
            assert!(matches!(
                validate_results(&engine, &left, &combined),
                Err(Error::Conflict(_))
            ));
        }
    }

    #[test]
    fn accepts_unique_rename_and_groups_ambiguous_duplicate_identity() {
        let directory = tempfile::tempdir().expect("tempdir");
        let engine = Engine::open(directory.path()).expect("engine");
        let base = checkpoint(&engine, None, &[("old", Some(b"same"))]);
        let renamed = checkpoint(
            &engine,
            Some(&base),
            &[("old", None), ("new", Some(b"same"))],
        );
        let unique = build(&engine, &engine, Some(&base), &renamed, &base).expect("plan");
        assert!(unique.conflicts.is_empty());
        assert!(unique.files.is_empty());

        let duplicate_base = checkpoint(
            &engine,
            None,
            &[("old", Some(b"same")), ("twin", Some(b"same"))],
        );
        let duplicate_rename = checkpoint(
            &engine,
            Some(&duplicate_base),
            &[("old", None), ("new", Some(b"same"))],
        );
        let ambiguous = build(
            &engine,
            &engine,
            Some(&duplicate_base),
            &duplicate_rename,
            &duplicate_base,
        )
        .expect("plan");
        assert_eq!(kinds(&ambiguous), vec![ConflictKind::Rename]);
        assert_eq!(
            ambiguous.conflicts[0]
                .paths
                .iter()
                .map(|path| path.path.as_str())
                .collect::<Vec<_>>(),
            vec!["new", "old", "twin"]
        );
    }

    #[test]
    fn groups_rename_edit_delete_and_different_rename_targets() {
        let directory = tempfile::tempdir().expect("tempdir");
        let engine = Engine::open(directory.path()).expect("engine");
        let base = checkpoint(&engine, None, &[("old", Some(b"same"))]);
        let renamed = checkpoint(
            &engine,
            Some(&base),
            &[("old", None), ("new", Some(b"same"))],
        );
        let edited = checkpoint(&engine, Some(&base), &[("old", Some(b"edited"))]);
        let deleted = checkpoint(&engine, Some(&base), &[("old", None)]);
        let other_rename = checkpoint(
            &engine,
            Some(&base),
            &[("old", None), ("other", Some(b"same"))],
        );

        for (other, expected_paths) in [
            (&edited, vec!["new", "old"]),
            (&deleted, vec!["new", "old"]),
            (&other_rename, vec!["new", "old", "other"]),
        ] {
            let plan = build(&engine, &engine, Some(&base), &renamed, other).expect("plan");
            assert_eq!(kinds(&plan), vec![ConflictKind::Rename]);
            assert_eq!(
                plan.conflicts[0]
                    .paths
                    .iter()
                    .map(|path| path.path.as_str())
                    .collect::<Vec<_>>(),
                expected_paths
            );
        }
    }

    #[test]
    fn reversed_disjoint_sides_produce_the_same_tree() {
        let directory = tempfile::tempdir().expect("tempdir");
        let engine = Engine::open(directory.path()).expect("engine");
        let base = checkpoint(&engine, None, &[("a", Some(b"a0")), ("b", Some(b"b0"))]);
        let left = checkpoint(&engine, Some(&base), &[("a", Some(b"a1"))]);
        let right = checkpoint(&engine, Some(&base), &[("b", Some(b"b1"))]);

        let forward = build(&engine, &engine, Some(&base), &left, &right).expect("forward");
        let reverse = build(&engine, &engine, Some(&base), &right, &left).expect("reverse");
        let forward_revision = apply_plan(&engine, &left, &right, &forward);
        let reverse_revision = apply_plan(&engine, &right, &left, &reverse);
        let forward_tree = engine
            .load_revision(&forward_revision)
            .expect("forward revision")
            .tree;
        let reverse_tree = engine
            .load_revision(&reverse_revision)
            .expect("reverse revision")
            .tree;
        assert_eq!(forward_tree, reverse_tree);
    }

    struct MissingFile<'a> {
        inner: &'a Engine,
        path: &'a str,
        missing: String,
    }

    impl SourceRead for MissingFile<'_> {
        fn read_encoded(&self, id: &str) -> Result<Vec<u8>> {
            self.inner.read_encoded(id)
        }

        fn load_revision(&self, id: &str) -> Result<Revision> {
            self.inner.load_revision(id)
        }

        fn read_file(&self, revision: &str, path: &str) -> Result<Vec<u8>> {
            if path == self.path {
                Err(Error::MissingObject(self.missing.clone()))
            } else {
                self.inner.read_file(revision, path)
            }
        }

        fn list_files(
            &self,
            revision_id: &str,
            cursor: Option<&str>,
            limit: usize,
        ) -> Result<FilePage> {
            self.inner.list_files(revision_id, cursor, limit)
        }

        fn diff(&self, left: &str, right: &str, limit: usize) -> Result<DiffResult> {
            self.inner.diff(left, right, limit)
        }

        fn diff_page(
            &self,
            left: &str,
            right: &str,
            cursor: Option<&str>,
            limit: usize,
        ) -> Result<DiffPage> {
            self.inner.diff_page(left, right, cursor, limit)
        }
    }

    #[test]
    fn missing_text_bytes_propagate_without_partial_success() {
        let directory = tempfile::tempdir().expect("tempdir");
        let engine = Engine::open(directory.path()).expect("engine");
        let base = checkpoint(&engine, None, &[("file", Some(b"base\n"))]);
        let left = checkpoint(&engine, Some(&base), &[("file", Some(b"left\n"))]);
        let right = checkpoint(&engine, Some(&base), &[("file", Some(b"right\n"))]);
        let missing = "a".repeat(64);
        let source = MissingFile {
            inner: &engine,
            path: "file",
            missing: missing.clone(),
        };

        match build(&source, &engine, Some(&base), &left, &right) {
            Err(Error::MissingObject(id)) => assert_eq!(id, missing),
            result => panic!("expected missing object, got {result:?}"),
        }
    }
}
