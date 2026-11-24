//! Offline qualification for reservation-region replay.
//!
//! The ignored test uses only pinned commits already present in the local Git
//! object database. Run it manually with:
//! `cargo test --test integration reservation_replay::replay_pinned_reservation_history -- --ignored --nocapture`.

#[path = "../../tools/qualification/reservation_regions.rs"]
mod regions;

use std::collections::BTreeSet;
use std::process::Command;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Aggregate {
    Overlap,
    Separate,
    Unknown,
}

fn aggregate<'a>(comparisons: impl IntoIterator<Item = Option<&'a str>>) -> Aggregate {
    let mut saw_comparison = false;
    let mut saw_unknown = false;
    for comparison in comparisons {
        saw_comparison = true;
        match comparison {
            Some("overlap") => return Aggregate::Overlap,
            Some("separate") => {}
            Some("unknown") | None => saw_unknown = true,
            Some(other) => panic!("unexpected region comparison {other:?}"),
        }
    }
    if saw_comparison && !saw_unknown {
        Aggregate::Separate
    } else {
        Aggregate::Unknown
    }
}

#[test]
fn aggregation_prefers_overlap_then_unknown_then_separate() {
    assert_eq!(
        aggregate([Some("separate"), Some("overlap"), None]),
        Aggregate::Overlap
    );
    assert_eq!(
        aggregate([Some("separate"), Some("unknown")]),
        Aggregate::Unknown
    );
    assert_eq!(
        aggregate([Some("separate"), Some("separate")]),
        Aggregate::Separate
    );
}

#[test]
fn aggregation_treats_missing_records_as_unknown() {
    assert_eq!(aggregate(std::iter::empty()), Aggregate::Unknown);
    assert_eq!(aggregate([None]), Aggregate::Unknown);
}

#[test]
fn synthetic_footprints_distinguish_regions() {
    let base = "fn alpha() {\n    one();\n}\n\nfn beta() {\n    two();\n}\n";
    let alpha = "fn alpha() {\n    changed();\n}\n\nfn beta() {\n    two();\n}\n";
    let beta = "fn alpha() {\n    one();\n}\n\nfn beta() {\n    changed();\n}\n";
    let alpha_again = "fn alpha() {\n    other();\n}\n\nfn beta() {\n    two();\n}\n";

    let alpha = regions::classify(base, base, alpha, &[(2, 1, 2, 1)]);
    let beta = regions::classify(base, base, beta, &[(6, 1, 6, 1)]);
    let alpha_again = regions::classify(base, base, alpha_again, &[(2, 1, 2, 1)]);

    assert_eq!(alpha.compare(&beta), "separate");
    assert_eq!(alpha.compare(&alpha_again), "overlap");
}

struct Fixture {
    label: &'static str,
    left: &'static str,
    right: &'static str,
    common_base: &'static str,
    expected: Aggregate,
    shared_paths: &'static [&'static str],
    overlap_witness: Option<&'static str>,
}

const FIXTURES: &[Fixture] = &[
    Fixture {
        label: "retrospective diff: #609 and #615",
        left: "8ec0cd7444173485ade8b477d0f3307cdacb4a51",
        right: "25d9b43e1cf8bfa578c85d0759a8dc0a05571c53",
        common_base: "a68c0855ba7d84e36780dea68da2f3c9addc0b26",
        expected: Aggregate::Overlap,
        shared_paths: &[
            "docs/agents.md",
            "src/hook_cli.rs",
            "src/hub/hook.rs",
            "tests/integration/agent_gates.rs",
        ],
        overlap_witness: Some("src/hook_cli.rs"),
    },
    Fixture {
        label: "retrospective diff: #625 and #632",
        left: "6628e72c0f43267a2321f7579f2ea5342471e571",
        right: "3756288f8d0f02f03a9fa51c5f7cf61141c41fde",
        common_base: "2a4c5f77ff83b60deaedac3e2c1ab32ea2f1ab97",
        expected: Aggregate::Separate,
        shared_paths: &["src/hub/mcp.rs"],
        overlap_witness: None,
    },
    Fixture {
        label: "retrospective diff: #602 and #607",
        left: "63a00c74181f2d63d30ccde3ba8755acc7078f52",
        right: "9f48a9c52807fd087bed8c56c427737d6e03feaa",
        common_base: "811106d80ff5864fc36451bc4d81dca40357b598",
        expected: Aggregate::Separate,
        shared_paths: &["src/hub/mcp.rs"],
        overlap_witness: None,
    },
];

#[test]
#[ignore = "manual offline qualification against pinned local Git history"]
fn replay_pinned_reservation_history() {
    let mut failures = Vec::new();
    for fixture in FIXTURES {
        match replay(fixture) {
            Ok((actual, paths)) => {
                println!("{}: {actual:?} across {}", fixture.label, paths.join(", "));
                if actual != fixture.expected {
                    failures.push(format!(
                        "{}: expected {:?}, got {actual:?} across {}",
                        fixture.label,
                        fixture.expected,
                        paths.join(", ")
                    ));
                }
            }
            Err(error) => failures.push(format!("{}: {error}", fixture.label)),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

fn replay(fixture: &Fixture) -> Result<(Aggregate, Vec<String>), String> {
    let left_parent = rev_parse(&format!("{}^", fixture.left))?;
    let right_parent = rev_parse(&format!("{}^", fixture.right))?;
    let common_base = git(&["merge-base", &left_parent, &right_parent])?
        .trim_end()
        .to_owned();
    if common_base != fixture.common_base || common_base != left_parent {
        return Err(format!(
            "common base {common_base} did not match pinned {} and older squash parent {left_parent}",
            fixture.common_base
        ));
    }

    let left_paths = changed_paths(&left_parent, fixture.left)?;
    let right_paths = changed_paths(&right_parent, fixture.right)?;
    let shared: Vec<_> = left_paths.intersection(&right_paths).cloned().collect();
    let expected_paths: Vec<_> = fixture
        .shared_paths
        .iter()
        .map(|path| (*path).to_owned())
        .collect();
    if shared != expected_paths {
        return Err(format!(
            "shared paths changed: expected {expected_paths:?}, got {shared:?}"
        ));
    }

    let mut comparisons = Vec::new();
    for path in &shared {
        let comparison = footprint_pair(
            &common_base,
            &left_parent,
            fixture.left,
            &right_parent,
            fixture.right,
            path,
        );
        println!("  {path}: {comparison}");
        if fixture.overlap_witness == Some(path.as_str()) && comparison != "overlap" {
            return Err(format!(
                "expected {path} to be an overlap witness, got {comparison}"
            ));
        }
        comparisons.push(Some(comparison));
    }
    Ok((aggregate(comparisons), shared))
}

fn footprint_pair(
    common_base: &str,
    left_parent: &str,
    left: &str,
    right_parent: &str,
    right: &str,
    path: &str,
) -> &'static str {
    // The region classifier intentionally handles Rust only. A shared non-Rust
    // path and any absent blob/hunk record are unknown evidence, not separation.
    if !path.ends_with(".rs") {
        return "unknown";
    }
    let Some((base, left_before, left_after, right_before, right_after)) =
        blobs(common_base, left_parent, left, right_parent, right, path)
    else {
        return "unknown";
    };
    let (Ok(left_hunks), Ok(right_hunks)) = (
        hunks(left_parent, left, path),
        hunks(right_parent, right, path),
    ) else {
        return "unknown";
    };
    let left = regions::classify(&base, &left_before, &left_after, &left_hunks);
    let right = regions::classify(&base, &right_before, &right_after, &right_hunks);
    left.compare(&right)
}

fn blobs(
    common_base: &str,
    left_parent: &str,
    left: &str,
    right_parent: &str,
    right: &str,
    path: &str,
) -> Option<(String, String, String, String, String)> {
    Some((
        blob(common_base, path).ok()?,
        blob(left_parent, path).ok()?,
        blob(left, path).ok()?,
        blob(right_parent, path).ok()?,
        blob(right, path).ok()?,
    ))
}

fn changed_paths(before: &str, after: &str) -> Result<BTreeSet<String>, String> {
    Ok(git(&[
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--name-only",
        before,
        after,
        "--",
    ])?
    .lines()
    .map(str::to_owned)
    .collect())
}

fn rev_parse(revision: &str) -> Result<String, String> {
    git(&["rev-parse", "--verify", revision]).map(|value| value.trim_end().to_owned())
}

fn blob(revision: &str, path: &str) -> Result<String, String> {
    git(&["show", &format!("{revision}:{path}")])
}

fn hunks(
    before: &str,
    after: &str,
    path: &str,
) -> Result<Vec<(usize, usize, usize, usize)>, String> {
    let patch = git(&[
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--diff-algorithm=myers",
        "--indent-heuristic",
        "--inter-hunk-context=0",
        "--unified=0",
        before,
        after,
        "--",
        path,
    ])?;
    patch
        .lines()
        .filter(|line| line.starts_with("@@ "))
        .map(parse_hunk)
        .collect()
}

fn parse_hunk(header: &str) -> Result<(usize, usize, usize, usize), String> {
    let mut fields = header.split_whitespace();
    if fields.next() != Some("@@") {
        return Err(format!("malformed hunk header {header:?}"));
    }
    let old = fields
        .next()
        .ok_or_else(|| format!("missing old range in {header:?}"))?;
    let new = fields
        .next()
        .ok_or_else(|| format!("missing new range in {header:?}"))?;
    parse_range(old.strip_prefix('-'), header).and_then(|(old_start, old_len)| {
        parse_range(new.strip_prefix('+'), header)
            .map(|(new_start, new_len)| (old_start, old_len, new_start, new_len))
    })
}

fn parse_range(range: Option<&str>, header: &str) -> Result<(usize, usize), String> {
    let range = range.ok_or_else(|| format!("malformed range in {header:?}"))?;
    let (start, len) = range.split_once(',').unwrap_or((range, "1"));
    Ok((
        start
            .parse()
            .map_err(|_| format!("bad range in {header:?}"))?,
        len.parse()
            .map_err(|_| format!("bad range in {header:?}"))?,
    ))
}

fn git(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .args(args)
        .output()
        .map_err(|error| format!("could not run git {}: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8(output.stdout)
        .map_err(|error| format!("git {} emitted non-UTF-8 output: {error}", args.join(" ")))
}
