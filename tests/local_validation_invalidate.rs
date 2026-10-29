//! The `Invalidate stale successes` pre-setup step: identical in every workflow copy, and run
//! against a fake `gh` that applies each `--jq` expression with the real `jq`. Synthetic data only.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const STEP: &str = "- name: Invalidate stale successes";
const FILES: [&str; 3] = [
    ".github/workflows/local-validation.yml",
    ".github/workflows/local-validation-reconcile.yml",
    "docs/advanced/local-validation.md",
];
const FAKE_GH: &str = r#"#!/bin/bash
method=GET; jqx=.; path=
while (($#)); do
  case $1 in
    -X) method=$2; shift ;;
    --jq) jqx=$2; shift ;;
    -f) shift ;;
    api | --paginate) ;;
    *) path=$1 ;;
  esac
  shift
done
case "$method $path" in
  "GET repos/o/r/pulls") [ -z "${LIST_FAIL:-}" ] || exit 1; jq -r "$jqx" "$FIX/pulls.json" ;;
  "GET repos/o/r/commits/"*"/status") s=${path#*commits/}; jq -r "$jqx" "$FIX/status-${s%/status}.json" ;;
  "POST repos/o/r/statuses/"*) echo "${path##*/}" >> "$FIX/posts"
    echo "{\"state\":\"${POST_REPLY:-pending}\"}" | jq -r "$jqx" ;;
  *) echo "unexpected call: $method $path" >&2; exit 1 ;;
esac
"#;

fn extract(path: &str) -> String {
    let text = fs::read_to_string(format!("{}/{path}", env!("CARGO_MANIFEST_DIR"))).unwrap();
    let mut lines = text.lines().skip_while(|l| l.trim() != STEP);
    assert!(lines.next().is_some(), "{path}: step not found");
    let mut lines = lines.skip_while(|l| l.trim() != "run: |");
    let run = lines.next().expect("run block");
    let indent = run.len() - run.trim_start().len() + 2;
    let body: Vec<&str> = lines
        .take_while(|l| l.trim().is_empty() || l.len() - l.trim_start().len() >= indent)
        .map(|l| l.get(indent..).unwrap_or(""))
        .collect();
    body.join("\n").trim_end().to_owned() + "\n"
}

fn sha(c: char) -> String {
    c.to_string().repeat(40)
}

fn status(total: usize, statuses: &[(&str, &str)]) -> String {
    let items: Vec<String> = statuses
        .iter()
        .map(|(c, s)| format!(r#"{{"context":"{c}","state":"{s}"}}"#))
        .collect();
    format!(
        r#"{{"total_count":{total},"statuses":[{}]}}"#,
        items.join(",")
    )
}

/// Runs the step; returns (success, sha values POSTed). `None` if `jq` is missing.
fn run(
    pulls: &str,
    statuses: &[(String, String)],
    envs: &[(&str, &str)],
) -> Option<(bool, Vec<String>)> {
    if Command::new("jq").arg("--version").output().is_err() {
        eprintln!("skipping: jq is not installed");
        return None;
    }
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let gh = bin.join("gh");
    fs::write(&gh, FAKE_GH).unwrap();
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.path().join("pulls.json"), pulls).unwrap();
    for (s, body) in statuses {
        fs::write(dir.path().join(format!("status-{s}.json")), body).unwrap();
    }
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let out = Command::new("bash")
        .args(["-c", &extract(FILES[0])])
        .env("PATH", path)
        .env("FIX", dir.path())
        .env("REPO", "o/r")
        .env("DEFAULT_BRANCH", "main")
        .env("RUN_URL", "https://example.invalid/run")
        .envs(envs.iter().copied())
        .output()
        .unwrap();
    let posts = fs::read_to_string(dir.path().join("posts")).unwrap_or_default();
    Some((
        out.status.success(),
        posts.lines().map(str::to_owned).collect(),
    ))
}

fn pulls(shas: &[String]) -> String {
    let items: Vec<String> = shas
        .iter()
        .map(|s| format!(r#"{{"head":{{"sha":{s}}}}}"#))
        .collect();
    format!("[{}]", items.join(","))
}

fn quoted(c: char) -> String {
    format!("\"{}\"", sha(c))
}

#[test]
fn step_is_identical_in_every_copy() {
    let first = extract(FILES[0]);
    assert!(first.starts_with("set -euo pipefail\n"));
    for f in &FILES[1..] {
        assert_eq!(extract(f), first, "{f}");
    }
}

#[test]
fn only_success_and_unknown_are_set_pending_once_each() {
    let ours = "agentscm/local-validation";
    let statuses = vec![
        (sha('a'), status(1, &[(ours, "success")])),
        (sha('b'), status(1, &[("other", "success")])),
        (sha('c'), status(1, &[(ours, "pending")])),
        (
            sha('d'),
            status(2, &[("other", "success"), (ours, "failure")]),
        ),
        (sha('e'), status(150, &[("other", "success")])),
    ];
    // 'a' is listed twice.
    let list = pulls(&['a', 'b', 'c', 'd', 'e', 'a'].map(quoted));
    let Some((ok, posts)) = run(&list, &statuses, &[]) else {
        return;
    };
    assert!(ok);
    assert_eq!(posts, [sha('a'), sha('e')]);
}

#[test]
fn invalid_or_null_sha_blocks_all_writes() {
    for bad in ["null", "\"ABC\"", "\"not-a-sha\""] {
        let list = format!(
            r#"[{{"head":{{"sha":{}}}}},{{"head":{{"sha":{bad}}}}}]"#,
            quoted('a')
        );
        let statuses = vec![(
            sha('a'),
            status(1, &[("agentscm/local-validation", "success")]),
        )];
        let Some((ok, posts)) = run(&list, &statuses, &[]) else {
            return;
        };
        assert!(!ok && posts.is_empty(), "{bad}");
    }
}

#[test]
fn listing_failure_blocks_all_writes() {
    let Some((ok, posts)) = run("[]", &[], &[("LIST_FAIL", "1")]) else {
        return;
    };
    assert!(!ok && posts.is_empty());
}

#[test]
fn unexpected_write_reply_fails_the_step() {
    let statuses = vec![(
        sha('a'),
        status(1, &[("agentscm/local-validation", "success")]),
    )];
    let list = pulls(&[quoted('a')]);
    let Some((ok, _)) = run(&list, &statuses, &[("POST_REPLY", "success")]) else {
        return;
    };
    assert!(!ok);
}
