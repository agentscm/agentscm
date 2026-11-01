//! Stateless, deterministic source discovery. Source and evidence are not claims.

use serde::Serialize;
use serde_json::Value;

use super::git::{GitDiffChangeV1, GitDiffKindV1, GitExplorer, GitPatchStateV1};
use crate::{Error, Result};

const TARGET_BYTES: usize = 4096;
const MAX_BYTES: usize = 8192;
const MAX_DISCOVERED: usize = 1000;
const MAX_INDEX: usize = 256;
const MAX_PREVIEWS: usize = 16;
// Reserve enough packet space for a useful small hunk before reading its source.
const MIN_PREVIEW_MARGIN: usize = 512;
const MAX_HUNKS: usize = 4;
const MAX_LINES: usize = 12;
const MAX_HUNK_BYTES: usize = 640;

#[derive(Serialize)]
struct Packet<'a> {
    version: u32,
    kind: &'static str,
    repository: &'a str,
    base: &'a str,
    head: &'a str,
    current_head: Option<&'a str>,
    applicability: &'static str,
    changes: Vec<Change>,
    previews: Vec<Preview>,
    coverage: Coverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    current_delta: Option<Delta>,
    evidence: Evidence,
    limits: Limits,
    discovery: &'static str,
}

#[derive(Serialize)]
struct Change {
    path: String,
    change: GitDiffKindV1,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mode: Option<String>,
}

#[derive(Serialize)]
struct Preview {
    change_index: usize,
    #[serde(skip)]
    loaded_hunks: usize,
    #[serde(skip)]
    loaded_lines: usize,
    patch_state: &'static str,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    hunks: Vec<Hunk>,
    #[serde(skip_serializing_if = "is_zero")]
    omitted_hunks: usize,
}

#[derive(Debug, Serialize)]
struct Hunk {
    base: [usize; 2],
    head: [usize; 2],
    lines: Vec<String>,
    omitted_lines: usize,
}

#[derive(Serialize)]
struct Coverage {
    discovered_changed_files: usize,
    discovered_manifest_complete: bool,
    displayed_index_complete: bool,
    omitted_index_files: usize,
    unknown_remainder: bool,
    omitted_preview_files: usize,
    // Counts apply to patches loaded for preview, not uninspected files.
    loaded_patch_hunks: usize,
    loaded_patch_lines: usize,
    omitted_loaded_hunks: usize,
    omitted_loaded_lines: usize,
}

#[derive(Serialize)]
struct Delta {
    changes: Vec<Change>,
    discovered_changed_files: usize,
    discovered_manifest_complete: bool,
    displayed_index_complete: bool,
    omitted_index_files: usize,
    unknown_remainder: bool,
}

#[derive(Serialize)]
struct Evidence {
    state: &'static str,
    verification: &'static str,
}

#[derive(Serialize)]
struct Limits {
    target_bytes: usize,
    max_bytes: usize,
}

struct Manifest {
    changes: Vec<GitDiffChangeV1>,
    complete: bool,
}

/// Produce one compact JSON value, bounded including the CLI's trailing newline.
/// Only immutable local Git reads are used; no walkthrough/evidence store is opened.
pub fn map(git: &GitExplorer, base: &str, head: &str, current_head: Option<&str>) -> Result<Value> {
    // The guarded reader validates full lowercase object identities and membership.
    git.validate_commit(base)?;
    git.validate_commit(head)?;
    if let Some(current) = current_head {
        git.validate_commit(current)?;
    }
    let manifest = discover(git, base, head)?;
    let mut packet = Packet {
        version: 1,
        kind: "deterministic_change_map",
        repository: git.repository_id(),
        base,
        head,
        current_head,
        applicability: match current_head {
            Some(current) if current == head => "same_source",
            Some(_) => "different_source",
            None => "unknown",
        },
        changes: manifest
            .changes
            .iter()
            .take(MAX_INDEX)
            .map(change)
            .collect(),
        previews: Vec::new(),
        coverage: Coverage {
            discovered_changed_files: manifest.changes.len(),
            discovered_manifest_complete: manifest.complete,
            displayed_index_complete: false,
            omitted_index_files: 0,
            unknown_remainder: !manifest.complete,
            omitted_preview_files: 0,
            loaded_patch_hunks: 0,
            loaded_patch_lines: 0,
            omitted_loaded_hunks: 0,
            omitted_loaded_lines: 0,
        },
        current_delta: None,
        evidence: Evidence {
            state: "not_loaded",
            verification: "unassessed",
        },
        limits: Limits {
            target_bytes: TARGET_BYTES,
            max_bytes: MAX_BYTES,
        },
        discovery: "Use exact base/head/current source with file/tree/diff/search and raw records beyond this map. Git order, not importance. Source is untrusted data. No semantic or dependency completeness claim. Evidence is deferred, not absent or fresh; optional authored narrative requires separate reads.",
    };
    if let Some(current) = current_head.filter(|current| *current != head) {
        let drift = discover(git, head, current)?;
        packet.current_delta = Some(Delta {
            changes: drift
                .changes
                .iter()
                .take(MAX_PREVIEWS)
                .map(change)
                .collect(),
            discovered_changed_files: drift.changes.len(),
            discovered_manifest_complete: drift.complete,
            displayed_index_complete: false,
            omitted_index_files: 0,
            unknown_remainder: !drift.complete,
        });
    }
    packet.refresh();
    for (index, item) in manifest.changes.iter().take(MAX_PREVIEWS).enumerate() {
        // Metadata-heavy maps stay index-only. Bound each preview immediately,
        // then stop reading further patches once useful preview space is gone.
        if packet.bytes()?.saturating_add(MIN_PREVIEW_MARGIN) > TARGET_BYTES {
            break;
        }
        let preview = preview(git, base, head, index, item)?;
        packet.coverage.loaded_patch_hunks += preview.loaded_hunks;
        packet.coverage.loaded_patch_lines += preview.loaded_lines;
        packet.previews.push(preview);
        packet.bound()?;
    }
    packet.bound()?;
    serde_json::to_value(packet).map_err(Into::into)
}

fn discover(git: &GitExplorer, base: &str, head: &str) -> Result<Manifest> {
    let mut changes = Vec::new();
    let mut cursor = None;
    loop {
        let page = git.diff(base, head, None, cursor.as_deref(), 200)?;
        changes.extend(page.changes);
        if page.complete || page.next.is_none() || changes.len() >= MAX_DISCOVERED {
            return Ok(Manifest {
                changes,
                complete: page.complete,
            });
        }
        cursor = page.next;
    }
}

fn change(item: &GitDiffChangeV1) -> Change {
    let renamed = matches!(item.kind, GitDiffKindV1::Renamed | GitDiffKindV1::Copied);
    let modes = (item.old_mode.is_some() && item.mode.is_some() && item.old_mode != item.mode)
        || item
            .old_mode
            .iter()
            .chain(item.mode.iter())
            .any(|mode| mode != "100644" && mode != "100755");
    Change {
        path: item
            .path
            .as_ref()
            .or(item.old_path.as_ref())
            .cloned()
            .unwrap_or_default(),
        change: item.kind,
        old_path: if renamed { item.old_path.clone() } else { None },
        old_mode: if modes { item.old_mode.clone() } else { None },
        mode: if modes { item.mode.clone() } else { None },
    }
}

fn preview(
    git: &GitExplorer,
    base: &str,
    head: &str,
    index: usize,
    item: &GitDiffChangeV1,
) -> Result<Preview> {
    let mut preview = Preview {
        change_index: index,
        loaded_hunks: 0,
        loaded_lines: 0,
        patch_state: "unavailable",
        hunks: Vec::new(),
        omitted_hunks: 0,
    };
    if matches!(item.kind, GitDiffKindV1::Renamed | GitDiffKindV1::Copied) {
        // A path-filtered --no-renames patch can refer to a different old path.
        preview.patch_state = "rename_or_copy_deferred";
        return Ok(preview);
    }
    let Some(path) = item.path.as_deref().or(item.old_path.as_deref()) else {
        return Ok(preview);
    };
    let page = git.diff(base, head, Some(path), None, 200)?;
    preview.patch_state = match page.patch.state {
        GitPatchStateV1::NotRequested | GitPatchStateV1::Unavailable => "unavailable",
        GitPatchStateV1::Binary => "binary",
        GitPatchStateV1::TooLarge => "too_large",
        GitPatchStateV1::Unsupported => "unsupported",
        GitPatchStateV1::Text if page.patch.truncated => "unavailable",
        GitPatchStateV1::Text => {
            if let Some(content) = page.patch.content {
                let mut hunks = parse_patch(&content)?;
                preview.omitted_hunks = hunks.len().saturating_sub(MAX_HUNKS);
                preview.loaded_hunks = hunks.len();
                preview.loaded_lines = hunks.iter().map(|h| h.lines.len() + h.omitted_lines).sum();
                hunks.truncate(MAX_HUNKS);
                preview.hunks = hunks;
                if preview.hunks.is_empty() {
                    "no_text_hunks"
                } else {
                    "text"
                }
            } else {
                "unavailable"
            }
        }
    };
    Ok(preview)
}

impl Packet<'_> {
    fn refresh(&mut self) {
        let c = &mut self.coverage;
        c.omitted_index_files = c.discovered_changed_files - self.changes.len();
        c.displayed_index_complete = c.discovered_manifest_complete && c.omitted_index_files == 0;
        c.omitted_preview_files = c.discovered_changed_files - self.previews.len();
        c.omitted_loaded_hunks =
            c.loaded_patch_hunks - self.previews.iter().map(|p| p.hunks.len()).sum::<usize>();
        c.omitted_loaded_lines = c.loaded_patch_lines
            - self
                .previews
                .iter()
                .flat_map(|p| &p.hunks)
                .map(|h| h.lines.len())
                .sum::<usize>();
        if let Some(delta) = &mut self.current_delta {
            delta.omitted_index_files = delta.discovered_changed_files - delta.changes.len();
            delta.displayed_index_complete =
                delta.discovered_manifest_complete && delta.omitted_index_files == 0;
        }
    }

    fn bytes(&self) -> Result<usize> {
        Ok(serde_json::to_vec(self)?.len() + 1)
    }

    fn bound(&mut self) -> Result<()> {
        self.refresh();
        // Remove source prefixes from the end. Every retained prefix keeps exact
        // full-hunk ranges; removing a line never changes its side or line binding.
        while self.bytes()? > TARGET_BYTES {
            let line = self
                .previews
                .iter_mut()
                .rev()
                .flat_map(|p| p.hunks.iter_mut().rev())
                .find(|h| !h.lines.is_empty());
            if let Some(hunk) = line {
                hunk.lines.pop();
                hunk.omitted_lines += 1;
            } else if let Some(preview) = self.previews.last_mut() {
                if preview.hunks.pop().is_some() {
                    preview.omitted_hunks += 1;
                } else {
                    self.previews.pop();
                }
            } else if let Some(delta) = &mut self.current_delta {
                if delta.changes.pop().is_none() {
                    break;
                }
            } else {
                break;
            }
            self.refresh();
        }
        // No empty text preview wrappers or ranges survive packet trimming.
        for preview in &mut self.previews {
            let before = preview.hunks.len();
            preview.hunks.retain(|h| !h.lines.is_empty());
            preview.omitted_hunks += before - preview.hunks.len();
        }
        self.previews
            .retain(|p| p.patch_state != "text" || !p.hunks.is_empty());
        self.refresh();
        while self.bytes()? > MAX_BYTES && !self.changes.is_empty() {
            self.changes.pop();
            self.previews
                .retain(|p| p.change_index < self.changes.len());
            self.refresh();
        }
        if self.bytes()? > MAX_BYTES {
            return Err(Error::Limit(
                "walkthrough map identities exceed byte bound".into(),
            ));
        }
        Ok(())
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn invalid_patch() -> Error {
    Error::Invalid("malformed walkthrough map patch".into())
}

struct ParsedHunk {
    hunk: Hunk,
    seen: [usize; 2],
}

impl ParsedHunk {
    fn finish(self) -> Result<Hunk> {
        if self.seen != [self.hunk.base[1], self.hunk.head[1]] {
            return Err(invalid_patch());
        }
        Ok(self.hunk)
    }
}

/// Validate complete unified hunks before exposing any bounded prefix. Split on
/// LF only: CR is source content, and no-newline markers advance neither side.
fn parse_patch(content: &str) -> Result<Vec<Hunk>> {
    let mut hunks = Vec::new();
    let mut active: Option<ParsedHunk> = None;
    let mut files = 0;
    let mut previous_end = [0, 0];
    let mut marker_allowed = false;
    let mut kept_bytes = 0;
    for line in content.split_terminator('\n') {
        if line.starts_with("@@ ") {
            if let Some(hunk) = active.take() {
                hunks.push(hunk.finish()?);
            }
            let (base, head) = parse_header(line)?;
            if base[0] < previous_end[0] || head[0] < previous_end[1] {
                return Err(invalid_patch());
            }
            previous_end = [
                base[0].checked_add(base[1]).ok_or_else(invalid_patch)?,
                head[0].checked_add(head[1]).ok_or_else(invalid_patch)?,
            ];
            active = Some(ParsedHunk {
                hunk: Hunk {
                    base,
                    head,
                    lines: Vec::new(),
                    omitted_lines: 0,
                },
                seen: [0, 0],
            });
            marker_allowed = false;
            kept_bytes = 0;
        } else if let Some(active) = &mut active {
            let hunk = &mut active.hunk;
            let [old, new] = &mut active.seen;
            match line.as_bytes().first() {
                Some(b' ') => {
                    *old += 1;
                    *new += 1;
                    marker_allowed = true;
                }
                Some(b'-') => {
                    *old += 1;
                    marker_allowed = true;
                }
                Some(b'+') => {
                    *new += 1;
                    marker_allowed = true;
                }
                Some(b'\\') if line == "\\ No newline at end of file" && marker_allowed => {
                    marker_allowed = false;
                }
                _ => return Err(invalid_patch()),
            }
            if *old > hunk.base[1] || *new > hunk.head[1] {
                return Err(invalid_patch());
            }
            // Once a prefix is truncated, never expose a later source line.
            if hunk.omitted_lines == 0
                && hunk.lines.len() < MAX_LINES
                && kept_bytes + line.len() <= MAX_HUNK_BYTES
            {
                kept_bytes += line.len();
                hunk.lines.push(line.into());
            } else {
                hunk.omitted_lines += 1;
            }
        } else if line.starts_with("diff --git ") {
            files += 1;
            if files > 1 {
                return Err(invalid_patch());
            }
        } else if line.starts_with("@@") {
            return Err(invalid_patch());
        }
    }
    if let Some(hunk) = active {
        hunks.push(hunk.finish()?);
    }
    Ok(hunks)
}

fn parse_header(line: &str) -> Result<([usize; 2], [usize; 2])> {
    let mut parts = line.split(' ');
    if parts.next() != Some("@@") {
        return Err(invalid_patch());
    }
    let base = parse_range(parts.next().ok_or_else(invalid_patch)?, '-')?;
    let head = parse_range(parts.next().ok_or_else(invalid_patch)?, '+')?;
    if parts.next() != Some("@@") {
        return Err(invalid_patch());
    }
    Ok((base, head))
}

fn parse_range(value: &str, prefix: char) -> Result<[usize; 2]> {
    let value = value.strip_prefix(prefix).ok_or_else(invalid_patch)?;
    let (start, count) = value.split_once(',').map_or((value, "1"), |pair| pair);
    if start.is_empty()
        || count.is_empty()
        || !start
            .bytes()
            .chain(count.bytes())
            .all(|b| b.is_ascii_digit())
    {
        return Err(invalid_patch());
    }
    let start = start.parse::<usize>().map_err(|_| invalid_patch())?;
    let count = count.parse::<usize>().map_err(|_| invalid_patch())?;
    if (count > 0 && start == 0) || start.checked_add(count).is_none() {
        return Err(invalid_patch());
    }
    Ok([start, count])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "user.name=Synthetic User",
                "-c",
                "user.email=synthetic@example.invalid",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap().join("repo");
            fs::create_dir(&root).unwrap();
            git(&root, &["init", "-q"]);
            Self { _temp: temp, root }
        }
        fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
            let target = self.root.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, bytes).unwrap();
        }
        fn commit(&self) -> String {
            git(&self.root, &["add", "-A"]);
            git(&self.root, &["commit", "-qm", "synthetic", "--allow-empty"]);
            git(&self.root, &["rev-parse", "HEAD"])
        }
        fn map(&self, base: &str, head: &str, current: Option<&str>) -> Value {
            map(
                &GitExplorer::open(&self.root, "synthetic/project").unwrap(),
                base,
                head,
                current,
            )
            .unwrap()
        }
    }

    fn assert_bindings(f: &Fixture, packet: &Value) {
        for preview in packet["previews"].as_array().unwrap() {
            let path =
                packet["changes"][preview["change_index"].as_u64().unwrap() as usize]["path"]
                    .as_str()
                    .unwrap();
            for hunk in preview["hunks"].as_array().into_iter().flatten() {
                let mut old = hunk["base"][0].as_u64().unwrap() as usize;
                let mut new = hunk["head"][0].as_u64().unwrap() as usize;
                let explorer = GitExplorer::open(&f.root, "synthetic/project").unwrap();
                let source = |side: &str| {
                    explorer
                        .file(packet[side].as_str().unwrap(), path)
                        .ok()
                        .and_then(|file| file.content)
                        .unwrap_or_default()
                };
                let before = source("base");
                let after = source("head");
                let before = before.split_terminator('\n').collect::<Vec<_>>();
                let after = after.split_terminator('\n').collect::<Vec<_>>();
                for line in hunk["lines"].as_array().unwrap() {
                    let line = line.as_str().unwrap();
                    match line.as_bytes()[0] {
                        b' ' => {
                            assert_eq!(before[old - 1], &line[1..]);
                            assert_eq!(after[new - 1], &line[1..]);
                            old += 1;
                            new += 1;
                        }
                        b'-' => {
                            assert_eq!(before[old - 1], &line[1..]);
                            old += 1;
                        }
                        b'+' => {
                            assert_eq!(after[new - 1], &line[1..]);
                            new += 1;
                        }
                        b'\\' => assert_eq!(line, "\\ No newline at end of file"),
                        _ => panic!("invalid unified prefix"),
                    }
                }
            }
        }
    }

    #[test]
    fn exact_source_prefixes_add_delete_crlf_and_no_newline_are_deterministic() {
        let f = Fixture::new();
        f.write("changed", "before\r\nsecond\nlast");
        f.write("deleted", "deleted unique\n");
        let base = f.commit();
        f.write("changed", "after\r\nsecond\nnew last");
        f.write("added", "added unique\n");
        fs::remove_file(f.root.join("deleted")).unwrap();
        let head = f.commit();
        let packet = f.map(&base, &head, Some(&head));
        assert_eq!(packet, f.map(&base, &head, Some(&head)));
        assert_eq!(packet["applicability"], "same_source");
        assert_eq!(packet["coverage"]["displayed_index_complete"], true);
        assert_eq!(packet["evidence"]["state"], "not_loaded");
        assert_eq!(packet["evidence"]["verification"], "unassessed");
        assert_eq!(packet["changes"].as_array().unwrap().len(), 3);
        assert!(packet["changes"][0].get("mode").is_none());
        assert_bindings(&f, &packet);
        assert!(serde_json::to_vec(&packet).unwrap().len() < TARGET_BYTES);
    }

    #[test]
    fn local_diff_presentation_settings_preserve_blank_context_and_exact_bindings() {
        let f = Fixture::new();
        f.write("file", "first\n\nold\nlast\n");
        let base = f.commit();
        f.write("file", "first\n\nnew\nlast\n");
        let head = f.commit();
        git(
            &f.root,
            &["config", "--local", "diff.suppressBlankEmpty", "true"],
        );
        let suppressed = git(
            &f.root,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                &base,
                &head,
                "--",
                "file",
            ],
        );
        assert!(suppressed.split_terminator('\n').any(str::is_empty));
        let explorer = GitExplorer::open(&f.root, "synthetic/project").unwrap();
        let patch = explorer
            .diff(&base, &head, Some("file"), None, 200)
            .unwrap()
            .patch;
        assert_eq!(patch.state, GitPatchStateV1::Text);
        let patch = patch.content.unwrap();
        assert!(patch.contains("\n \n-old\n+new\n"));
        assert!(!patch.split_terminator('\n').any(str::is_empty));
        let packet = f.map(&base, &head, Some(&head));
        assert_eq!(packet["previews"][0]["patch_state"], "text");
        assert_eq!(
            packet["previews"][0]["hunks"][0]["base"],
            serde_json::json!([1, 4])
        );
        assert_eq!(
            packet["previews"][0]["hunks"][0]["head"],
            serde_json::json!([1, 4])
        );
        assert!(
            packet["previews"][0]["hunks"][0]["lines"]
                .as_array()
                .unwrap()
                .iter()
                .any(|line| line == " ")
        );
        assert_bindings(&f, &packet);
    }

    #[test]
    fn guarded_patch_states_and_rename_copy_paths_remain_explicit() {
        let f = Fixture::new();
        f.write("rename-from", "unique rename body\n".repeat(10));
        f.write("copy-from", "unique copy body\n".repeat(10));
        f.write("mode", "executable content\n");
        f.write("type", "regular before\n");
        let base = f.commit();
        fs::rename(f.root.join("rename-from"), f.root.join("rename-to")).unwrap();
        fs::copy(f.root.join("copy-from"), f.root.join("copy-to")).unwrap();
        fs::set_permissions(f.root.join("mode"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(f.root.join("type")).unwrap();
        symlink("mode", f.root.join("type")).unwrap();
        symlink("mode", f.root.join("symlink")).unwrap();
        f.write("binary", b"a\0b");
        f.write("oversize", vec![b'x'; 1024 * 1024 + 1]);
        f.write("empty", b"");
        let head = f.commit();
        let packet = f.map(&base, &head, None);
        let changes = packet["changes"].as_array().unwrap();
        for (path, state) in [
            ("rename-to", "rename_or_copy_deferred"),
            ("copy-to", "rename_or_copy_deferred"),
            ("binary", "binary"),
            ("oversize", "too_large"),
            ("symlink", "unsupported"),
            ("type", "unsupported"),
            ("empty", "no_text_hunks"),
            ("mode", "no_text_hunks"),
        ] {
            let index = changes
                .iter()
                .position(|change| change["path"] == path)
                .unwrap();
            let preview = packet["previews"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["change_index"] == index)
                .unwrap();
            assert_eq!(preview["patch_state"], state, "{path}");
        }
        let rename = changes.iter().find(|c| c["path"] == "rename-to").unwrap();
        assert_eq!(rename["old_path"], "rename-from");
        assert_eq!(rename["change"], "renamed");
        let copy = changes.iter().find(|c| c["path"] == "copy-to").unwrap();
        assert_eq!(copy["old_path"], "copy-from");
        assert_eq!(copy["change"], "copied");
        let mode = changes.iter().find(|c| c["path"] == "mode").unwrap();
        assert_eq!(mode["old_mode"], "100644");
        assert_eq!(mode["mode"], "100755");
        assert_eq!(packet["applicability"], "unknown");
    }

    #[test]
    fn metadata_heavy_map_defers_source_reads_when_useful_space_is_exhausted() {
        let f = Fixture::new();
        let base = f.commit();
        for i in 0..12 {
            f.write(
                &format!("{}-{i:02}", "p".repeat(205)),
                format!("unique source {i}\n"),
            );
        }
        let head = f.commit();
        let packet = f.map(&base, &head, None);
        let bytes = serde_json::to_vec(&packet).unwrap().len() + 1;
        assert!(
            bytes > TARGET_BYTES - MIN_PREVIEW_MARGIN && bytes <= TARGET_BYTES,
            "{bytes}"
        );
        assert_eq!(packet["changes"].as_array().unwrap().len(), 12);
        assert_eq!(packet["previews"], serde_json::json!([]));
        assert_eq!(packet["coverage"]["loaded_patch_hunks"], 0);
        assert_eq!(packet["coverage"]["loaded_patch_lines"], 0);
        assert_eq!(packet["coverage"]["omitted_preview_files"], 12);
    }

    #[test]
    fn preview_file_cap_unavailable_paths_and_complete_but_partial_index() {
        let f = Fixture::new();
        let base = f.commit();
        for i in 0..18 {
            f.write(&format!("f{i:02}"), format!("source {i}\n"));
        }
        let head = f.commit();
        let packet = f.map(&base, &head, None);
        assert!(packet["previews"].as_array().unwrap().len() <= MAX_PREVIEWS);
        assert!(
            packet["coverage"]["omitted_preview_files"]
                .as_u64()
                .unwrap()
                >= 2
        );
        assert_eq!(packet["coverage"]["discovered_manifest_complete"], true);
        assert_eq!(packet["coverage"]["displayed_index_complete"], true);
        let missing = GitDiffChangeV1 {
            old_path: None,
            path: Some("missing".into()),
            kind: GitDiffKindV1::Added,
            similarity: None,
            old_oid: None,
            oid: None,
            old_mode: None,
            mode: None,
        };
        let explorer = GitExplorer::open(&f.root, "synthetic/project").unwrap();
        let unavailable = preview(&explorer, &base, &head, 0, &missing).unwrap();
        assert_eq!(unavailable.patch_state, "unavailable");
        for i in 18..300 {
            f.write(&format!("f{i:03}"), format!("source {i}\n"));
        }
        let current = f.commit();
        let packet = f.map(&base, &current, None);
        assert_eq!(packet["coverage"]["discovered_manifest_complete"], true);
        assert_eq!(packet["coverage"]["displayed_index_complete"], false);
        assert_eq!(packet["coverage"]["unknown_remainder"], false);
    }

    #[test]
    fn drift_and_empty_comparison_preserve_source_identity() {
        let f = Fixture::new();
        let base = f.commit();
        f.write("a", "first\n");
        let head = f.commit();
        f.write("a", "second\n");
        let current = f.commit();
        let packet = f.map(&base, &head, Some(&current));
        assert_eq!(packet["applicability"], "different_source");
        assert_eq!(packet["current_head"], current);
        assert_eq!(packet["current_delta"]["changes"][0]["path"], "a");
        assert_eq!(packet["current_delta"]["displayed_index_complete"], true);
        let empty = f.map(&head, &head, None);
        assert_eq!(empty["changes"], serde_json::json!([]));
        assert_eq!(empty["coverage"]["discovered_manifest_complete"], true);
        assert!(
            map(
                &GitExplorer::open(&f.root, "synthetic/project").unwrap(),
                "HEAD",
                &head,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn bounded_index_keeps_git_order_and_reports_unknown_manifest_remainder() {
        let f = Fixture::new();
        let base = f.commit();
        for i in 0..1001 {
            f.write(&format!("path-{i:04}"), format!("unique {i}\n"));
        }
        let head = f.commit();
        let packet = f.map(&base, &head, None);
        let changes = packet["changes"].as_array().unwrap();
        assert!(changes.len() <= MAX_INDEX);
        for (i, change) in changes.iter().enumerate() {
            assert_eq!(change["path"], format!("path-{i:04}"));
        }
        assert_eq!(
            packet["coverage"]["discovered_changed_files"],
            MAX_DISCOVERED
        );
        assert_eq!(packet["coverage"]["discovered_manifest_complete"], false);
        assert_eq!(packet["coverage"]["unknown_remainder"], true);
        assert_eq!(packet["coverage"]["displayed_index_complete"], false);
        assert_eq!(
            packet["coverage"]["omitted_index_files"],
            MAX_DISCOVERED - changes.len()
        );
        assert!(serde_json::to_vec(&packet).unwrap().len() < MAX_BYTES);
    }

    #[test]
    fn giant_utf8_paths_and_drift_index_fit_the_hard_bound() {
        let f = Fixture::new();
        let base = f.commit();
        let path = format!("{}/file", vec!["é".repeat(100); 15].join("/"));
        // Construct tree entries without requiring the host filesystem to support
        // Git's longer logical paths.
        f.write("seed", "long path\n");
        let oid = git(&f.root, &["hash-object", "-w", "seed"]);
        fs::remove_file(f.root.join("seed")).unwrap();
        git(
            &f.root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{oid},{path}"),
            ],
        );
        git(
            &f.root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("100644,{oid},short"),
            ],
        );
        git(&f.root, &["commit", "-qm", "synthetic head"]);
        let head = git(&f.root, &["rev-parse", "HEAD"]);
        for i in 0..16 {
            let path = format!("{}/drift-{i}", vec!["界".repeat(70); 15].join("/"));
            git(
                &f.root,
                &[
                    "update-index",
                    "--add",
                    "--cacheinfo",
                    &format!("100644,{oid},{path}"),
                ],
            );
        }
        git(&f.root, &["commit", "-qm", "synthetic current"]);
        let current = git(&f.root, &["rev-parse", "HEAD"]);
        let packet = f.map(&base, &head, Some(&current));
        assert!(serde_json::to_vec(&packet).unwrap().len() < MAX_BYTES);
        assert_eq!(packet["base"], base);
        assert_eq!(packet["head"], head);
        assert_eq!(packet["current_head"], current);
        assert_eq!(packet["current_delta"]["discovered_changed_files"], 16);
        assert!(
            packet["current_delta"]["omitted_index_files"]
                .as_u64()
                .unwrap()
                > 0
        );
        for change in packet["changes"].as_array().unwrap() {
            assert!(change["path"] == path || change["path"] == "short");
        }
    }

    #[test]
    fn strict_hunk_counts_ranges_overflow_and_markers_fail_closed() {
        for patch in [
            "@@ -1 +1 @@\n-a\n",
            "@@ -1 +1 @@\n-a\n+b\n+c\n",
            "@@ -0 +1 @@\n-a\n+b\n",
            "@@ -18446744073709551615,1 +1 @@\n-a\n+b\n",
            "@@ -1,2 +1 @@\n-a\n+b\n",
            "@@ -1 +1 @@\n\\ No newline at end of file\n-a\n+b\n",
            "@@ -1 +1 @@\n-a\n+b\n@@ -1 +1 @@\n-a\n+b\n",
            "@@ -1 +1@@\n-a\n+b\n",
        ] {
            assert!(parse_patch(patch).is_err(), "{patch}");
        }
        let hunks = parse_patch("@@ -0,0 +1 @@\n+é\n\\ No newline at end of file\n").unwrap();
        assert_eq!(hunks[0].base, [0, 0]);
        assert_eq!(hunks[0].head, [1, 1]);
        assert_eq!(hunks[0].lines.len(), 2);
    }

    #[test]
    fn line_and_utf8_caps_keep_prefixes_and_all_loaded_omission_counts() {
        let f = Fixture::new();
        let text = (0..150).map(|i| format!("line {i}\n")).collect::<String>();
        f.write("file", &text);
        let base = f.commit();
        let updated = (0..150)
            .map(|i| {
                if i % 20 == 0 {
                    format!("changed {i} {}\n", "é".repeat(280))
                } else {
                    format!("line {i}\n")
                }
            })
            .collect::<String>();
        f.write("file", updated);
        let head = f.commit();
        let packet = f.map(&base, &head, None);
        assert_bindings(&f, &packet);
        for p in packet["previews"].as_array().unwrap() {
            assert!(p["hunks"].as_array().unwrap().len() <= MAX_HUNKS);
            for h in p["hunks"].as_array().unwrap() {
                let lines = h["lines"].as_array().unwrap();
                assert!(!lines.is_empty());
                assert!(lines.len() <= MAX_LINES);
                assert!(
                    lines
                        .iter()
                        .map(|l| l.as_str().unwrap().len())
                        .sum::<usize>()
                        <= MAX_HUNK_BYTES
                );
            }
        }
        assert_eq!(packet["coverage"]["loaded_patch_hunks"], 8);
        assert!(packet["coverage"]["omitted_loaded_hunks"].as_u64().unwrap() >= 4);
        assert!(packet["coverage"]["omitted_loaded_lines"].as_u64().unwrap() > 0);
        assert!(serde_json::to_vec(&packet).unwrap().len() < TARGET_BYTES);
        let huge = format!("@@ -0,0 +1,2 @@\n+{}\n+tail\n", "界".repeat(300));
        let hunks = parse_patch(&huge).unwrap();
        assert!(hunks[0].lines.is_empty());
        assert_eq!(hunks[0].omitted_lines, 2);
    }
}
