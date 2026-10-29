#![cfg(unix)]

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier};

use agentscm::Error;
use agentscm::repo_explorer::git::{
    GitDiffKindV1, GitEntryKindV1, GitExplorer, GitFileStateV1, GitPatchStateV1, GitPathHistoryV1,
    GitRefKindV1, PageLimitStateV1,
};
use tempfile::TempDir;

struct Repository {
    temp: TempDir,
    root: PathBuf,
}

impl Repository {
    fn new() -> Self {
        Self::new_with_format("sha1")
    }

    fn new_with_format(format: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir(&root).unwrap();
        let object_format = format!("--object-format={format}");
        git(&root, &["init", "-q", object_format.as_str(), "-b", "main"]);
        git(&root, &["config", "user.name", "Explorer Test"]);
        git(&root, &["config", "user.email", "explorer@example.invalid"]);
        Self { temp, root }
    }

    fn write(&self, path: &str, content: &[u8]) {
        let target = self.root.join(path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(target, content).unwrap();
    }

    fn commit_all(&self, subject: &str, timestamp: i64) -> String {
        git(&self.root, &["add", "-A"]);
        let date = format!("{timestamp} +0000");
        let status = Command::new("git")
            .current_dir(&self.root)
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args(["commit", "-q", "-m", subject])
            .status()
            .unwrap();
        assert!(status.success());
        rev(&self.root, "HEAD")
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn rev(root: &Path, name: &str) -> String {
    let output = Command::new("git")
        .current_dir(root)
        .args(["rev-parse", name])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

#[test]
fn immutable_browse_reports_dirty_state_and_bounded_file_states() {
    let repository = Repository::new();
    repository.write("README.md", b"first\nneedle[fixed]\n");
    repository.write("src/main.rs", b"fn main() {}\n");
    repository.write("binary.dat", b"a\0b");
    repository.write("large.txt", &vec![b'x'; 1024 * 1024 + 1]);
    repository.write("<script>.txt", b"markup stays source text\n");
    repository.write("run.sh", b"#!/bin/sh\nexit 0\n");
    let mut permissions = fs::metadata(repository.root.join("run.sh"))
        .unwrap()
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(repository.root.join("run.sh"), permissions).unwrap();
    symlink("README.md", repository.root.join("readme-link")).unwrap();
    let first = repository.commit_all("first", 1_792_800_000);

    let explorer = GitExplorer::open(&repository.root, "test/repository").unwrap();
    let summary = explorer.summary(None).unwrap();
    assert_eq!(summary.commit, first);
    assert_eq!(summary.tree.len(), first.len());
    assert_eq!(summary.observed_head.as_deref(), Some(first.as_str()));
    assert_eq!(summary.head_ref.as_deref(), Some("refs/heads/main"));
    assert_eq!(summary.dirty, None);
    assert!(summary.refs.iter().any(|reference| {
        reference.name == "refs/heads/main" && reference.kind == GitRefKindV1::LocalBranch
    }));

    let tree = explorer.tree(&first, None, None, 2).unwrap();
    assert_eq!(tree.entries.len(), 2);
    assert_eq!(tree.limit_state, PageLimitStateV1::MoreResults);
    assert!(!tree.complete);
    let next = tree.next.as_deref().unwrap();
    let second_page = explorer.tree(&first, None, Some(next), 2).unwrap();
    assert!(!second_page.entries.is_empty());

    let readme = explorer.file(&first, "README.md").unwrap();
    assert_eq!(readme.state, GitFileStateV1::Text);
    assert_eq!(readme.content.as_deref(), Some("first\nneedle[fixed]\n"));
    assert_eq!(readme.kind, GitEntryKindV1::File);
    assert_eq!(
        explorer.file(&first, "binary.dat").unwrap().state,
        GitFileStateV1::Binary
    );
    assert_eq!(
        explorer.file(&first, "large.txt").unwrap().state,
        GitFileStateV1::TooLarge
    );
    assert_eq!(
        explorer.file(&first, "readme-link").unwrap().state,
        GitFileStateV1::Unsupported
    );
    assert_eq!(
        explorer.file(&first, "run.sh").unwrap().kind,
        GitEntryKindV1::Executable
    );

    let search = explorer.search(&first, "needle[fixed]", None, 50).unwrap();
    assert_eq!(search.matches.len(), 1);
    assert_eq!(search.matches[0].path, "README.md");
    assert_eq!(search.matches[0].line, 2);
    assert!(search.complete);

    repository.write("README.md", b"second\n");
    let second = repository.commit_all("second", 1_792_800_100);
    repository.write("untracked.txt", b"dirty\n");
    let refreshed = explorer.summary(None).unwrap();
    assert_eq!(refreshed.commit, second);
    assert_eq!(refreshed.dirty, Some(true));
    assert_eq!(
        explorer.file(&first, "README.md").unwrap().content,
        Some("first\nneedle[fixed]\n".into())
    );
}

#[test]
fn history_and_diff_keep_exact_paths_and_bind_cursors() {
    let repository = Repository::new();
    repository.write("old.txt", b"one\ntwo\n");
    repository.write("other.txt", b"old\n");
    let first = repository.commit_all("first", 1_792_801_000);
    git(&repository.root, &["mv", "old.txt", "new.txt"]);
    repository.write("new.txt", b"one\ntwo changed\n");
    repository.write("other.txt", b"new\n");
    repository.write("third.txt", b"added\n");
    let second = repository.commit_all("rename and edit", 1_792_801_100);
    let explorer = GitExplorer::open(&repository.root, "test/history").unwrap();

    let history = explorer
        .history(&second, Some("new.txt"), None, 25)
        .unwrap();
    assert_eq!(history.path_interpretation, GitPathHistoryV1::Exact);
    assert_eq!(history.entries.len(), 1);
    assert_eq!(history.entries[0].oid, second);
    assert_eq!(history.entries[0].author_name, "Explorer Test");
    assert_eq!(history.entries[0].author_time, 1_792_801_100);
    assert_eq!(history.entries[0].subject, "rename and edit");

    let first_page = explorer.diff(&first, &second, None, None, 1).unwrap();
    assert_eq!(first_page.changes.len(), 1);
    assert_eq!(first_page.limit_state, PageLimitStateV1::MoreResults);
    assert_eq!(first_page.patch.state, GitPatchStateV1::NotRequested);
    let cursor = first_page.next.as_deref().unwrap();
    let second_page = explorer
        .diff(&first, &second, None, Some(cursor), 1)
        .unwrap();
    assert_eq!(second_page.changes.len(), 1);
    let wrong_binding = explorer.diff(&second, &first, None, Some(cursor), 1);
    assert!(matches!(wrong_binding, Err(Error::Invalid(_))));

    let selected = explorer
        .diff(&first, &second, Some("new.txt"), None, 100)
        .unwrap();
    assert!(selected.changes.iter().any(|change| {
        matches!(change.kind, GitDiffKindV1::Added | GitDiffKindV1::Renamed)
            && change.path.as_deref() == Some("new.txt")
    }));
    assert_eq!(selected.patch.state, GitPatchStateV1::Text);
    assert!(
        selected
            .patch
            .content
            .as_deref()
            .unwrap()
            .contains("two changed")
    );
    assert!(!selected.patch.truncated);
}

#[test]
fn history_truncates_oversized_strings_and_keeps_the_cursor() {
    let repository = Repository::new();
    repository.write("history.txt", b"old\n");
    let older = repository.commit_all("older", 1_792_801_200);
    repository.write("history.txt", b"new\n");
    git(&repository.root, &["add", "history.txt"]);
    let author = "A\\\"".repeat(700);
    let subject = "generated \\\"subject\\\" \\".repeat(700);
    let status = Command::new("git")
        .current_dir(&repository.root)
        .env("GIT_AUTHOR_NAME", &author)
        .env("GIT_AUTHOR_DATE", "1792801300 +0000")
        .env("GIT_COMMITTER_DATE", "1792801300 +0000")
        .args(["commit", "-q", "-m", subject.as_str()])
        .status()
        .unwrap();
    assert!(status.success());
    let newest = rev(&repository.root, "HEAD");
    let explorer = GitExplorer::open(&repository.root, "test/long-history").unwrap();

    let page = explorer.history(&newest, None, None, 1).unwrap();
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].oid, newest);
    assert!(page.entries[0].author_name.ends_with(" [truncated]"));
    assert!(page.entries[0].subject.ends_with(" [truncated]"));
    assert!(
        serde_json::to_string(&page.entries[0].author_name)
            .unwrap()
            .len()
            <= 1024
    );
    assert!(
        serde_json::to_string(&page.entries[0].subject)
            .unwrap()
            .len()
            <= 8 * 1024
    );
    assert_eq!(page.limit_state, PageLimitStateV1::MoreResults);
    let next = page.next.as_deref().unwrap();
    let older_page = explorer.history(&newest, None, Some(next), 1).unwrap();
    assert_eq!(older_page.entries[0].oid, older);
}

#[test]
fn summary_uses_partial_status_without_running_repository_filters() {
    let repository = Repository::new();
    repository.write(
        ".gitattributes",
        b"clean.txt filter=cleanfixture\nprocess.txt filter=processfixture\n",
    );
    repository.write("clean.txt", b"before\n");
    repository.write("process.txt", b"old\n");
    repository.write("info.txt", b"left\n");
    let commit = repository.commit_all("filtered", 1_792_801_400);
    fs::write(
        repository.root.join(".git/info/attributes"),
        b"info.txt filter=cleanfixture\n",
    )
    .unwrap();

    let clean_script = repository.temp.path().join("clean-filter.sh");
    let process_script = repository.temp.path().join("process-filter.sh");
    let clean_marker = repository.temp.path().join("clean-filter.sh.marker");
    let process_marker = repository.temp.path().join("process-filter.sh.marker");
    fs::write(
        &clean_script,
        b"#!/bin/sh\nprintf executed > \"$0.marker\"\ncat\n",
    )
    .unwrap();
    fs::write(
        &process_script,
        b"#!/bin/sh\nprintf executed > \"$0.marker\"\nexit 1\n",
    )
    .unwrap();
    for script in [&clean_script, &process_script] {
        let mut permissions = fs::metadata(script).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(script, permissions).unwrap();
    }
    let clean_command = clean_script.to_string_lossy().into_owned();
    let process_command = process_script.to_string_lossy().into_owned();
    git(
        &repository.root,
        &["config", "filter.cleanfixture.clean", &clean_command],
    );
    git(
        &repository.root,
        &["config", "filter.cleanfixture.required", "true"],
    );
    git(
        &repository.root,
        &["config", "filter.processfixture.process", &process_command],
    );
    git(
        &repository.root,
        &["config", "filter.processfixture.required", "true"],
    );

    repository.write("clean.txt", b"after!\n");
    repository.write("process.txt", b"new\n");
    repository.write("info.txt", b"rite\n");
    let explorer = GitExplorer::open(&repository.root, "test/safe-status").unwrap();
    let summary = explorer.summary(None).unwrap();
    assert_eq!(summary.commit, commit);
    assert_eq!(summary.dirty, None);
    assert!(!clean_marker.exists());
    assert!(!process_marker.exists());

    repository.write("untracked.txt", b"observed\n");
    assert_eq!(explorer.summary(None).unwrap().dirty, Some(true));
    // Disable only the fixture filters for setup: index writes can refresh unrelated entries.
    // The repository configuration remains hostile for every explorer read below.
    git(
        &repository.root,
        &[
            "-c",
            "filter.cleanfixture.clean=",
            "-c",
            "filter.cleanfixture.required=false",
            "-c",
            "filter.processfixture.process=",
            "-c",
            "filter.processfixture.required=false",
            "add",
            "untracked.txt",
        ],
    );
    assert_eq!(explorer.summary(None).unwrap().dirty, Some(true));
    assert!(!clean_marker.exists());
    assert!(!process_marker.exists());
}

#[test]
fn truncated_untracked_observation_does_not_claim_dirty() {
    let repository = Repository::new();
    repository.write("tracked.txt", b"tracked\n");
    repository.commit_all("tracked", 1_792_801_450);
    let long_component = "x".repeat(230);
    for index in 0..4_600 {
        repository.write(&format!("bulk/{index:04}-{long_component}.txt"), b"");
    }
    let explorer = GitExplorer::open(&repository.root, "test/truncated-status").unwrap();
    assert_eq!(explorer.summary(None).unwrap().dirty, None);

    repository.write("staged.txt", b"staged\n");
    git(&repository.root, &["add", "staged.txt"]);
    assert_eq!(explorer.summary(None).unwrap().dirty, Some(true));
}

#[test]
fn summary_verifies_symbolic_head_beyond_the_ref_page() {
    let repository = Repository::new();
    repository.write("state.txt", b"one\n");
    let commit = repository.commit_all("one", 1_792_801_500);
    let mut child = Command::new("git")
        .current_dir(&repository.root)
        .args(["update-ref", "--stdin"])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let input = child.stdin.as_mut().unwrap();
        for index in 0..101 {
            writeln!(input, "update refs/heads/a{index:03} {commit}").unwrap();
        }
        writeln!(input, "update refs/heads/zzz {commit}").unwrap();
    }
    assert!(child.wait().unwrap().success());
    git(
        &repository.root,
        &["symbolic-ref", "HEAD", "refs/heads/zzz"],
    );

    let explorer = GitExplorer::open(&repository.root, "test/many-refs").unwrap();
    let summary = explorer.summary(None).unwrap();
    assert_eq!(summary.commit, commit);
    assert_eq!(summary.head_ref.as_deref(), Some("refs/heads/zzz"));
    assert!(summary.refs_omitted);
    assert!(
        summary
            .refs
            .iter()
            .all(|reference| reference.name != "refs/heads/zzz")
    );
}

#[test]
fn hostile_paths_full_oid_validation_and_root_pin_fail_closed() {
    let repository = Repository::new();
    repository.write("--not-an-option", b"literal option-like path\n");
    repository.write("markup/<img onerror=alert(1)>.txt", b"safe markup\n");
    repository.write("line\nbreak.txt", b"newline path\n");
    let commit = repository.commit_all("hostile paths", 1_792_802_000);
    let explorer = GitExplorer::open(&repository.root, "test/hostile").unwrap();

    assert_eq!(
        explorer
            .file(&commit, "--not-an-option")
            .unwrap()
            .content
            .as_deref(),
        Some("literal option-like path\n")
    );
    let markup = explorer.tree(&commit, Some("markup"), None, 100).unwrap();
    assert_eq!(markup.entries[0].name, "<img onerror=alert(1)>.txt");
    assert_eq!(
        explorer.file(&commit, "line\nbreak.txt").unwrap().state,
        GitFileStateV1::Text
    );
    assert!(matches!(
        explorer.file(&commit[..12], "--not-an-option"),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        explorer.file(&commit, "../outside"),
        Err(Error::Invalid(_))
    ));
    explorer.validate_commit(&commit).unwrap();

    let page = explorer.tree(&commit, None, None, 1).unwrap();
    let mut cursor = page.next.unwrap();
    cursor.push('x');
    assert!(matches!(
        explorer.tree(&commit, None, Some(&cursor), 1),
        Err(Error::Invalid(_))
    ));

    let moved = repository.temp.path().join("moved-checkout");
    fs::rename(&repository.root, &moved).unwrap();
    fs::create_dir(&repository.root).unwrap();
    git(&repository.root, &["init", "-q", "-b", "main"]);
    assert!(matches!(explorer.summary(None), Err(Error::Conflict(_))));
}

#[test]
fn sha256_and_shallow_repositories_preserve_available_identity() {
    let sha256 = Repository::new_with_format("sha256");
    sha256.write("sha256.txt", b"sixty-four hex characters\n");
    let sha256_commit = sha256.commit_all("sha256", 1_792_803_000);
    assert_eq!(sha256_commit.len(), 64);
    let explorer = GitExplorer::open(&sha256.root, "test/sha256").unwrap();
    explorer.validate_commit(&sha256_commit).unwrap();
    assert_eq!(
        explorer
            .file(&sha256_commit, "sha256.txt")
            .unwrap()
            .content
            .as_deref(),
        Some("sixty-four hex characters\n")
    );

    let source = Repository::new();
    source.write("history.txt", b"old\n");
    let old = source.commit_all("old", 1_792_803_100);
    source.write("history.txt", b"new\n");
    let head = source.commit_all("new", 1_792_803_200);
    let shallow_root = source.temp.path().join("shallow");
    let source_url = format!("file://{}", source.root.display());
    let shallow_arg = shallow_root.to_string_lossy().into_owned();
    let output = Command::new("git")
        .args([
            "clone",
            "-q",
            "--depth=1",
            source_url.as_str(),
            shallow_arg.as_str(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let shallow = GitExplorer::open(&shallow_root, "test/shallow").unwrap();
    let summary = shallow.summary(None).unwrap();
    assert_eq!(summary.commit, head);
    assert!(summary.shallow);
    let history = shallow.history(&head, None, None, 25).unwrap();
    assert!(history.shallow);
    assert_eq!(history.entries.len(), 1);
    assert!(matches!(
        shallow.validate_commit(&old),
        Err(Error::NotFound(_))
    ));
}

#[test]
fn summary_never_combines_two_observed_head_states() {
    let repository = Repository::new();
    repository.write("state.txt", b"one\n");
    let first = repository.commit_all("one", 1_792_804_000);
    repository.write("state.txt", b"two\n");
    let second = repository.commit_all("two", 1_792_804_100);
    let explorer = GitExplorer::open(&repository.root, "test/concurrent-summary").unwrap();
    let root = repository.root.clone();
    let start = Arc::new(Barrier::new(2));
    let writer_start = Arc::clone(&start);
    let first_for_writer = first.clone();
    let second_for_writer = second.clone();
    let writer = std::thread::spawn(move || {
        writer_start.wait();
        for index in 0..80 {
            let target = if index % 2 == 0 {
                first_for_writer.as_str()
            } else {
                second_for_writer.as_str()
            };
            git(&root, &["update-ref", "refs/heads/main", target]);
        }
    });
    start.wait();
    for _ in 0..20 {
        match explorer.summary(None) {
            Ok(summary) => {
                assert_eq!(
                    summary.observed_head.as_deref(),
                    Some(summary.commit.as_str())
                );
                let main = summary
                    .refs
                    .iter()
                    .find(|reference| reference.name == "refs/heads/main")
                    .unwrap();
                assert_eq!(main.commit, summary.commit);
            }
            Err(Error::Conflict(_)) => {}
            Err(error) => panic!("unexpected summary failure: {error}"),
        }
    }
    writer.join().unwrap();
}

#[test]
fn high_line_count_patch_stops_at_the_output_bound() {
    let repository = Repository::new();
    let mut left = Vec::with_capacity(990_000);
    let mut right = Vec::with_capacity(990_000);
    for _ in 0..90_000 {
        left.extend_from_slice(b"a123456789\n");
        right.extend_from_slice(b"b123456789\n");
    }
    repository.write("dense.txt", &left);
    let first = repository.commit_all("dense left", 1_792_805_000);
    repository.write("dense.txt", &right);
    let second = repository.commit_all("dense right", 1_792_805_100);
    let explorer = GitExplorer::open(&repository.root, "test/dense-patch").unwrap();
    let started = std::time::Instant::now();
    let diff = explorer
        .diff(&first, &second, Some("dense.txt"), None, 100)
        .unwrap();
    assert_eq!(diff.patch.state, GitPatchStateV1::TooLarge);
    assert!(diff.patch.content.is_none());
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}
