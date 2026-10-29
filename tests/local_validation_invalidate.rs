//! The `Invalidate stale successes` pre-setup step: identical in every workflow copy, and run
//! against a fake `gh` that applies each `--jq` expression with the real `jq`. Synthetic data only.
//! Production `gh --jq` uses gojq; this test uses jq, so only the shared filter subset is covered.
//! A status fixture may hold several JSON documents, one per page, which `jq` runs over in turn.
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
const OURS: &str = "agentscm/local-validation";
const FAKE_GH: &str = r#"#!/bin/bash
echo "$*" >> "$FIX/calls"
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
sha=${path#*/commits/}; sha=${sha%/status}
case "$method $path" in
  "GET repos/o/r/pulls") [ -z "${LIST_FAIL:-}" ] || exit 1; jq -r "$jqx" "$FIX/pulls.json" ;;
  "GET repos/o/r/commits/"*"/status") [ "${STATUS_FAIL:-}" != "$sha" ] || exit 1
    jq -r "$jqx" "$FIX/status-$sha.json" ;;
  "POST repos/o/r/statuses/"*) [ "${POST_FAIL:-}" != "${path##*/}" ] || exit 1
    echo "${path##*/}" >> "$FIX/posts"
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

/// One page of a combined status response.
fn status(statuses: &[(&str, &str)]) -> String {
    let items: Vec<String> = statuses
        .iter()
        .map(|(c, s)| format!(r#"{{"context":"{c}","state":"{s}"}}"#))
        .collect();
    format!(r#"{{"statuses":[{}]}}"#, items.join(","))
}

struct Outcome {
    ok: bool,
    posts: Vec<String>,
    calls: Vec<String>,
    stdout: String,
}

fn run(pulls: &str, statuses: &[(String, String)], envs: &[(&str, &str)]) -> Outcome {
    assert!(
        Command::new("jq").arg("--version").output().is_ok(),
        "jq is required for this test"
    );
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
        .env("RUN_URL", RUN_URL)
        .envs(envs.iter().copied())
        .output()
        .unwrap();
    let read = |n: &str| fs::read_to_string(dir.path().join(n)).unwrap_or_default();
    Outcome {
        ok: out.status.success(),
        posts: read("posts").lines().map(str::to_owned).collect(),
        calls: read("calls").lines().map(str::to_owned).collect(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
    }
}

const RUN_URL: &str = "https://example.invalid/run";

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

fn success_for(c: char) -> (String, String) {
    (sha(c), status(&[(OURS, "success")]))
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
fn only_success_is_set_pending_once_each_with_the_expected_arguments() {
    let statuses = vec![
        success_for('a'),
        (sha('b'), status(&[("other", "success")])),
        (sha('c'), status(&[(OURS, "pending")])),
        (sha('d'), status(&[("other", "success"), (OURS, "failure")])),
    ];
    // 'a' is listed twice.
    let list = pulls(&['a', 'b', 'c', 'd', 'a'].map(quoted));
    let o = run(&list, &statuses, &[]);
    assert!(o.ok);
    assert_eq!(o.posts, [sha('a')]);
    let listing = o
        .calls
        .iter()
        .find(|c| c.contains("repos/o/r/pulls"))
        .unwrap();
    assert!(
        listing.contains("state=open") && listing.contains("base=main"),
        "{listing}"
    );
    let post = o.calls.iter().find(|c| c.contains("-X POST")).unwrap();
    for want in [
        "state=pending",
        "context=agentscm/local-validation",
        &format!("target_url={RUN_URL}"),
    ] {
        assert!(post.contains(want), "{want} missing from {post}");
    }
}

#[test]
fn empty_listing_writes_nothing() {
    let o = run("[]", &[], &[]);
    assert!(o.ok && o.posts.is_empty());
}

#[test]
fn context_found_only_on_a_later_page() {
    let two_pages = format!(
        "{}\n{}",
        status(&[("other", "success")]),
        status(&[("more", "error"), (OURS, "success")])
    );
    let o = run(&pulls(&[quoted('a')]), &[(sha('a'), two_pages)], &[]);
    assert!(o.ok);
    assert_eq!(o.posts, [sha('a')]);
}

#[test]
fn failed_status_read_is_unknown_and_later_heads_still_run() {
    let statuses = vec![success_for('b')];
    let list = pulls(&[quoted('a'), quoted('b')]);
    let o = run(&list, &statuses, &[("STATUS_FAIL", &sha('a'))]);
    assert!(o.ok);
    assert_eq!(o.posts, [sha('a'), sha('b')]);
}

#[test]
fn refused_write_is_reported_and_later_heads_still_run() {
    let statuses = vec![success_for('a'), success_for('b')];
    let list = pulls(&[quoted('a'), quoted('b')]);
    let o = run(&list, &statuses, &[("POST_FAIL", &sha('a'))]);
    assert!(o.ok);
    assert_eq!(o.posts, [sha('b')]);
    assert!(
        o.stdout
            .contains(&format!("::error::failed to set {}", sha('a')))
    );
}

#[test]
fn invalid_or_null_sha_blocks_all_writes() {
    for bad in ["null", "\"ABC\"", "\"not-a-sha\""] {
        let list = format!(
            r#"[{{"head":{{"sha":{}}}}},{{"head":{{"sha":{bad}}}}}]"#,
            quoted('a')
        );
        let o = run(&list, &[success_for('a')], &[]);
        assert!(!o.ok && o.posts.is_empty(), "{bad}");
    }
}

#[test]
fn listing_failure_blocks_all_writes() {
    let o = run("[]", &[], &[("LIST_FAIL", "1")]);
    assert!(!o.ok && o.posts.is_empty());
}

#[test]
fn unexpected_write_reply_is_reported_without_failing_the_step() {
    let o = run(
        &pulls(&[quoted('a')]),
        &[success_for('a')],
        &[("POST_REPLY", "success")],
    );
    assert!(o.ok);
    assert!(o.stdout.contains("::error::unexpected reply"));
}
