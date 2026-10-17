//! `agentscm autopsy`: replays a person's own Claude Code edit history through the hub's
//! contested-file rule. Pure and local: it takes log lines and returns edits and near-misses.
//! Only a path, session, time and branch are kept from each line; file contents are never stored.
use std::collections::{BTreeMap, BTreeSet};
use std::io::BufRead;

use serde::Deserialize;

use crate::hub::github::parse_rfc3339;
use crate::hub::hook::relative;
use crate::hub::intent::spec_key;
use crate::hub::session::{HOLD_CAP_SECONDS, MAX_SESSION_PATHS, SESSION_TTL_SECONDS, never_hold};

/// Longer lines (a huge Write payload) are skipped without being stored.
const MAX_LINE_BYTES: usize = 32 * 1024 * 1024;
/// The tools the installed Claude Code hook matches (`Edit|Write|MultiEdit`).
const EDIT_TOOLS: [&str; 3] = ["Edit", "Write", "MultiEdit"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edit {
    pub session: String,
    /// Unix seconds.
    pub at: i64,
    /// Repository-relative, '/' separated.
    pub path: String,
    pub branch: Option<String>,
    /// False for lock and generated files: the hub never holds or contests them, but an edit
    /// to one still renews the session's live hold.
    pub held: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NearMiss {
    pub path: String,
    pub session: String,
    pub holder: String,
    pub at: i64,
    /// Seconds since the holder last touched the file.
    pub gap_secs: i64,
    pub branch: Option<String>,
    pub holder_branch: Option<String>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Replay {
    pub sessions: usize,
    pub edits: usize,
    pub near_misses: Vec<NearMiss>,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session: Option<String>,
    #[serde(rename = "gitBranch")]
    branch: Option<String>,
    message: Option<Message>,
}

#[derive(Deserialize)]
struct Message {
    content: Option<Vec<Block>>,
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    input: Option<Input>,
}

/// Only the path field: Edit and Write payloads are skipped by the parser, not kept.
#[derive(Deserialize)]
struct Input {
    file_path: Option<String>,
}

enum Next {
    End,
    /// Over `MAX_LINE_BYTES`; nothing was stored.
    Skipped,
    Line,
}

fn next_line(log: &mut impl BufRead, buf: &mut Vec<u8>) -> std::io::Result<Next> {
    buf.clear();
    let (mut seen, mut long) = (false, false);
    loop {
        let chunk = log.fill_buf()?;
        if chunk.is_empty() {
            break;
        }
        seen = true;
        let end = chunk.iter().position(|b| *b == b'\n');
        let take = end.map_or(chunk.len(), |i| i + 1);
        if buf.len() + take > MAX_LINE_BYTES {
            long = true;
            buf.clear();
        }
        if !long {
            buf.extend_from_slice(&chunk[..take]);
        }
        log.consume(take);
        if end.is_some() {
            break;
        }
    }
    Ok(match (seen, long) {
        (false, _) => Next::End,
        (true, true) => Next::Skipped,
        (true, false) => Next::Line,
    })
}

/// The hub's path rule: absolute, lexically under a root, and a valid repo-relative key.
fn repo_path(path: &str, roots: &[&str]) -> Option<String> {
    let absolute = path.starts_with(['/', '\\']) || path.as_bytes().get(1) == Some(&b':');
    let rel = relative(path, roots).filter(|_| absolute)?;
    spec_key(&rel).ok().map(|_| rel)
}

fn edits_of(line: &[u8], roots: &[&str], since: i64) -> Vec<Edit> {
    let Ok(line) = serde_json::from_slice::<Line>(line) else {
        return Vec::new();
    };
    let (Some("assistant"), Some(session), Some(at)) = (
        line.kind.as_deref(),
        line.session.filter(|s| !s.is_empty()),
        line.timestamp.as_deref().and_then(parse_rfc3339),
    ) else {
        return Vec::new();
    };
    if at < since {
        return Vec::new();
    }
    let blocks = line.message.and_then(|m| m.content).unwrap_or_default();
    blocks
        .into_iter()
        .filter(|b| b.kind.as_deref() == Some("tool_use"))
        .filter(|b| b.name.as_deref().is_some_and(|n| EDIT_TOOLS.contains(&n)))
        .filter_map(|b| b.input.and_then(|i| i.file_path))
        .filter_map(|path| repo_path(&path, roots))
        .map(|path| Edit {
            session: session.clone(),
            at,
            held: !never_hold(&path),
            path,
            branch: line.branch.clone(),
        })
        .collect()
}

/// The edits at or after `since` to files under one of `roots`, in log order. Malformed,
/// oversized and unrelated lines are skipped; a read error ends the log. Roots are tried
/// longest first, so a worktree inside the repository keeps its own relative paths.
pub fn read_edits(mut log: impl BufRead, roots: &[String], since: i64) -> Vec<Edit> {
    let mut roots: Vec<&str> = roots.iter().map(String::as_str).collect();
    roots.sort_by_key(|root| std::cmp::Reverse(root.len()));
    let (mut buf, mut edits) = (Vec::new(), Vec::new());
    while let Ok(next) = next_line(&mut log, &mut buf) {
        match next {
            Next::End => break,
            Next::Skipped => {}
            Next::Line => edits.extend(edits_of(&buf, &roots, since)),
        }
    }
    edits
}

/// One session's hold, as the hub keeps it: files with the time and branch of their last edit.
struct Hold {
    created: i64,
    expires: i64,
    files: BTreeMap<String, (i64, Option<String>)>,
}

/// Replays the edits through the hub's session rule: a session holds the files it edits for
/// `SESSION_TTL_SECONDS` from each edit, never past `HOLD_CAP_SECONDS` from its first. A
/// near-miss is the first edit, per session and file, to a file another session holds that the
/// editing session did not already hold itself (the hub warns, or refuses, only then).
///
/// The logs do not say when a session ended, and a session that ended early would have released
/// its files, so near-misses can be over-counted. Edits the logs do not contain (a deleted
/// worktree's, another agent's) can make it under-count.
pub fn replay(mut edits: Vec<Edit>) -> Replay {
    edits.sort_by_key(|edit| edit.at);
    let mut holds: BTreeMap<&str, Hold> = BTreeMap::new();
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut near_misses = Vec::new();
    for edit in &edits {
        let (session, path, t) = (edit.session.as_str(), edit.path.as_str(), edit.at);
        if !edit.held {
            if let Some(hold) = holds.get_mut(session).filter(|hold| hold.expires > t) {
                hold.expires = (t + SESSION_TTL_SECONDS).min(hold.created + HOLD_CAP_SECONDS);
            }
            continue;
        }
        let already_held = holds
            .get(session)
            .is_some_and(|hold| hold.expires > t && hold.files.contains_key(path));
        if !already_held {
            // The holder that touched the file last is the one the hub would name.
            let holder = holds
                .iter()
                .filter(|(other, hold)| **other != session && hold.expires > t)
                .filter_map(|(other, hold)| hold.files.get(path).map(|touch| (*other, touch)))
                .max_by_key(|(_, (at, _))| *at);
            if let Some((holder, (last_at, holder_branch))) = holder
                && seen.insert((session, path))
            {
                near_misses.push(NearMiss {
                    path: edit.path.clone(),
                    session: edit.session.clone(),
                    holder: holder.to_owned(),
                    at: t,
                    gap_secs: t - last_at,
                    branch: edit.branch.clone(),
                    holder_branch: holder_branch.clone(),
                });
            }
        }
        let hold = holds.entry(session).or_insert_with(|| Hold {
            created: t,
            expires: 0,
            files: BTreeMap::new(),
        });
        if hold.expires > t {
            hold.expires = (t + SESSION_TTL_SECONDS).min(hold.created + HOLD_CAP_SECONDS);
        } else {
            *hold = Hold {
                created: t,
                expires: t + SESSION_TTL_SECONDS,
                files: BTreeMap::new(),
            };
        }
        // The new file plus the smallest old ones, like the hub.
        let mut files = BTreeMap::from([(edit.path.clone(), (t, edit.branch.clone()))]);
        for (file, value) in std::mem::take(&mut hold.files) {
            if files.len() >= MAX_SESSION_PATHS {
                break;
            }
            files.entry(file).or_insert(value);
        }
        hold.files = files;
    }
    Replay {
        sessions: holds.len(),
        edits: edits.len(),
        near_misses,
    }
}

/// A file's near-misses: how many, the smallest gap, and how many sessions were involved.
#[derive(Debug, PartialEq, Eq)]
pub struct Ranked<'a> {
    pub path: &'a str,
    pub count: usize,
    pub gap_secs: i64,
    pub sessions: usize,
}

/// Files by near-miss count, most first; ties by smallest gap, then path.
pub fn ranked(near: &[NearMiss]) -> Vec<Ranked<'_>> {
    let mut by_file: BTreeMap<&str, (usize, i64, BTreeSet<&str>)> = BTreeMap::new();
    for miss in near {
        let entry = by_file
            .entry(&miss.path)
            .or_insert((0, i64::MAX, BTreeSet::new()));
        entry.0 += 1;
        entry.1 = entry.1.min(miss.gap_secs);
        entry
            .2
            .extend([miss.session.as_str(), miss.holder.as_str()]);
    }
    let mut ranked: Vec<_> = by_file
        .into_iter()
        .map(|(path, (count, gap_secs, sessions))| Ranked {
            path,
            count,
            gap_secs,
            sessions: sessions.len(),
        })
        .collect();
    ranked.sort_by_key(|r| (std::cmp::Reverse(r.count), r.gap_secs, r.path));
    ranked
}

pub fn worst(near: &[NearMiss]) -> Option<Ranked<'_>> {
    ranked(near).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(session: &str, at: i64, path: &str) -> Edit {
        Edit {
            session: session.into(),
            at,
            held: !never_hold(path),
            path: path.into(),
            branch: None,
        }
    }

    fn line(kind: &str, time: &str, tool: &str, input: &str) -> String {
        format!(
            r#"{{"type":"{kind}","timestamp":"{time}","sessionId":"s1","gitBranch":"feat","message":{{"content":[{{"type":"tool_use","name":"{tool}","input":{input}}}]}}}}"#
        )
    }

    fn roots(list: &[&str]) -> Vec<String> {
        list.iter().map(|root| (*root).to_owned()).collect()
    }

    #[test]
    fn reads_only_repository_edits() {
        let at = "2026-10-16T20:15:00.934Z";
        let edit_of = |tool: &str, path: &str| {
            line(
                "assistant",
                at,
                tool,
                &format!(r#"{{"file_path":"{path}","old_string":"SECRET","new_string":"x"}}"#),
            )
        };
        let log = [
            edit_of("Edit", "/work/repo/src/a.rs"),
            edit_of("Write", "/work/repo/b.txt"),
            edit_of("MultiEdit", "/work/repo/c/d.rs"),
            line(
                "assistant",
                at,
                "NotebookEdit",
                r#"{"notebook_path":"/work/repo/n.ipynb"}"#,
            ),
            line(
                "assistant",
                at,
                "Bash",
                r#"{"command":"ls /work/repo/x.rs"}"#,
            ),
            line("user", at, "Edit", r#"{"file_path":"/work/repo/user.rs"}"#),
            "{not json".into(),
            String::new(),
            edit_of("Edit", "src/relative.rs"),
            edit_of("Edit", "/elsewhere/e.rs"),
            edit_of("Edit", "/work/repo/../repo/dotdot.rs"),
            edit_of("Edit", "/work/repo/../outside.rs"),
            edit_of("Edit", "/work/repo/star*.rs"),
            edit_of("Edit", "/work/repo/Cargo.lock"),
            line(
                "assistant",
                "2026-10-01T00:00:00Z",
                "Edit",
                r#"{"file_path":"/work/repo/old.rs"}"#,
            ),
            line(
                "assistant",
                "garbage",
                "Edit",
                r#"{"file_path":"/work/repo/time.rs"}"#,
            ),
            edit_of("Edit", "/work/repo"),
        ]
        .join("\n");
        let since = parse_rfc3339("2026-10-10T00:00:00Z").unwrap();
        let got = read_edits(log.as_bytes(), &roots(&["/work/repo"]), since);
        let at = parse_rfc3339(at).unwrap();
        let kept: Vec<(&str, bool)> = got.iter().map(|e| (e.path.as_str(), e.held)).collect();
        assert_eq!(
            kept,
            [
                ("src/a.rs", true),
                ("b.txt", true),
                ("c/d.rs", true),
                ("dotdot.rs", true),
                ("Cargo.lock", false),
            ]
        );
        assert!(got.iter().all(|e| e.session == "s1" && e.at == at));
        assert!(got.iter().all(|e| e.branch.as_deref() == Some("feat")));
    }

    #[test]
    fn deepest_root_wins_and_long_lines_are_skipped() {
        let mut log = vec![b'x'; MAX_LINE_BYTES + 1];
        log.push(b'\n');
        log.extend(
            line(
                "assistant",
                "2026-10-16T20:15:00Z",
                "Edit",
                r#"{"file_path":"/r/wt/a.rs"}"#,
            )
            .bytes(),
        );
        let got = read_edits(log.as_slice(), &roots(&["/r", "/r/wt"]), 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "a.rs");
    }

    #[test]
    fn windows_roots_match_ignoring_case() {
        let log = line(
            "assistant",
            "2026-10-16T20:15:00Z",
            "Edit",
            r#"{"file_path":"c:\\Users\\Me\\Repo\\src\\a.rs"}"#,
        );
        let got = read_edits(log.as_bytes(), &roots(&["C:\\users\\me\\repo"]), 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].path, "src/a.rs");
    }

    fn pairs(replayed: &Replay) -> Vec<(&str, &str, i64)> {
        replayed
            .near_misses
            .iter()
            .map(|m| (m.session.as_str(), m.holder.as_str(), m.gap_secs))
            .collect()
    }

    #[test]
    fn two_sessions_four_minutes_apart_is_one_near_miss() {
        let r = replay(vec![edit("a", 1000, "f.rs"), edit("b", 1240, "f.rs")]);
        assert_eq!((r.sessions, r.edits), (2, 2));
        assert_eq!(pairs(&r), [("b", "a", 240)]);
    }

    #[test]
    fn no_near_miss_when_far_apart_the_same_session_or_another_file() {
        for edits in [
            vec![edit("a", 0, "f.rs"), edit("b", 1200, "f.rs")],
            vec![edit("a", 0, "f.rs"), edit("a", 60, "f.rs")],
            vec![edit("a", 0, "f.rs"), edit("b", 60, "g.rs")],
            // Lock files are never held or contested.
            vec![edit("a", 0, "Cargo.lock"), edit("b", 60, "Cargo.lock")],
        ] {
            assert!(replay(edits).near_misses.is_empty());
        }
    }

    #[test]
    fn activity_renews_the_hold_until_the_cap() {
        let near = |b_at| {
            replay(vec![
                edit("a", 0, "f.rs"),
                edit("a", 600, "g.rs"),
                edit("a", 1200, "g.rs"),
                edit("b", b_at, "f.rs"),
            ])
            .near_misses
            .len()
        };
        // Past 1200 seconds from a's last touch of f.rs, but a's activity kept the hold alive.
        assert_eq!(near(1500), 1);
        // The cap ends it 1800 seconds after a's first edit, though a is still active.
        assert_eq!(near(1800), 0);
    }

    #[test]
    fn editing_a_lock_file_still_renews_the_hold() {
        let r = replay(vec![
            edit("a", 0, "f.rs"),
            edit("a", 1100, "Cargo.lock"),
            edit("b", 1300, "f.rs"),
        ]);
        assert_eq!(pairs(&r), [("b", "a", 1300)]);
    }

    #[test]
    fn a_file_already_held_by_the_editing_session_is_not_contested_again() {
        // The hub warns only about files a call adds: b is warned once, a never.
        let r = replay(vec![
            edit("a", 0, "f.rs"),
            edit("b", 60, "f.rs"),
            edit("a", 120, "f.rs"),
        ]);
        assert_eq!(pairs(&r), [("b", "a", 60)]);
    }

    #[test]
    fn a_session_is_told_once_per_file() {
        // b was refused on f.rs while a held it; once a's and b's holds lapse and c holds it,
        // b's later edit is a repeat of the refused file, which the hub lets pass.
        let r = replay(vec![
            edit("a", 0, "f.rs"),
            edit("b", 60, "f.rs"),
            edit("c", 4000, "f.rs"),
            edit("b", 4060, "f.rs"),
        ]);
        assert_eq!(pairs(&r), [("b", "a", 60)]);
        // Two holders at once are one warning for the editing session.
        let r = replay(vec![
            edit("a", 0, "f.rs"),
            edit("b", 10, "f.rs"),
            edit("c", 20, "f.rs"),
        ]);
        assert_eq!(pairs(&r), [("b", "a", 10), ("c", "b", 10)]);
    }

    #[test]
    fn a_hold_keeps_the_new_file_and_the_smallest_old_ones() {
        let mut edits: Vec<Edit> = (0..60)
            .map(|i| edit("a", i, &format!("f{i:02}.rs")))
            .collect();
        edits.push(edit("a", 100, "z.rs"));
        // At the 50-file limit f59 is the first dropped; f00 is still held.
        edits.push(edit("b", 110, "f59.rs"));
        edits.push(edit("b", 111, "f00.rs"));
        assert_eq!(pairs(&replay(edits)), [("b", "a", 111)]);
    }

    #[test]
    fn worst_prefers_count_then_gap_then_path() {
        let miss = |path: &str, gap, session: &str| NearMiss {
            path: path.into(),
            session: session.into(),
            holder: "a".into(),
            at: 0,
            gap_secs: gap,
            branch: None,
            holder_branch: None,
        };
        let near = [
            miss("z", 5, "b"),
            miss("a", 50, "b"),
            miss("a", 60, "c"),
            miss("y", 5, "b"),
            miss("b", 5, "b"),
        ];
        let first = worst(&near).unwrap();
        assert_eq!(
            (first.path, first.count, first.gap_secs, first.sessions),
            ("a", 2, 50, 3)
        );
        let second = &ranked(&near)[1];
        assert_eq!(
            (second.path, second.count, second.gap_secs, second.sessions),
            ("b", 1, 5, 2)
        );
        assert_eq!(worst(&[]), None);
    }
}
