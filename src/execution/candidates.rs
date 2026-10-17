//! Agent candidates: a successful, controller-collected run hands back file changes on stdout
//! (`agentscm.candidate/v1`). The controller turns them into a native revision whose parent is the
//! run's exact subject revision. The agent is untrusted, so the result is proposed source only.
use std::collections::BTreeSet;

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::ExecutionOutcome;
use super::attempts::{read_command_attempt, verify_success};
use crate::model::FileChange;
use crate::store::Engine;
use crate::{Error, Result};

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateOutput {
    pub version: u32,
    pub changes: Vec<CandidateChange>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateChange {
    pub path: String,
    /// Lowercase hex of the file bytes; null deletes. Required, so a missing key is not a delete.
    #[serde(deserialize_with = "Option::deserialize")]
    pub hex: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct CommandCandidate {
    pub attempt_id: String,
    pub record_digest: String,
    pub base_revision: String,
    pub revision: String,
    pub changes: usize,
}

impl CandidateOutput {
    /// Exactly one JSON document and nothing else. Size and count limits are `Engine::checkpoint`'s.
    pub fn parse_changes(stdout: &[u8]) -> Result<Vec<FileChange>> {
        let output: Self = serde_json::from_slice(stdout)
            .map_err(|error| Error::Invalid(format!("stdout is not a valid candidate: {error}")))?;
        if output.version != 1 {
            return Err(Error::Invalid(format!(
                "unsupported candidate version {}",
                output.version
            )));
        }
        if output.changes.is_empty() {
            return Err(Error::Invalid("candidate has no changes".into()));
        }
        let mut seen = BTreeSet::new();
        output
            .changes
            .into_iter()
            .map(|change| {
                validate_path(&change.path)?;
                if !seen.insert(change.path.clone()) {
                    return Err(Error::Invalid(format!(
                        "candidate path appears twice: {}",
                        change.path
                    )));
                }
                let content = change.hex.as_deref().map(decode_hex).transpose()?;
                Ok(FileChange {
                    path: change.path,
                    content,
                })
            })
            .collect()
    }
}

/// Stricter than the engine: nothing that escapes the tree or uses a name Git export and
/// workspaces reserve (`.git`, `.agentscm`, ...), whatever its case.
fn validate_path(path: &str) -> Result<()> {
    let bad = path.starts_with('/')
        || path.contains(['\\', '\0'])
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || crate::git_path_profile::uses_reserved_namespace(path);
    if bad {
        return Err(Error::Invalid(format!("unsafe candidate path {path:?}")));
    }
    Ok(())
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        _ => Err(Error::Invalid(
            "candidate hex must be lowercase hexadecimal".into(),
        )),
    };
    if !value.len().is_multiple_of(2) {
        return Err(Error::Invalid("candidate hex has odd length".into()));
    }
    value
        .as_bytes()
        .chunks(2)
        .map(|pair| Ok(nibble(pair[0])? << 4 | nibble(pair[1])?))
        .collect()
}

/// No table: the binding is reproducible from the immutable record, because the same record
/// always yields the same revision (content-addressed checkpoint on the same parent).
pub(crate) fn command_candidate(
    connection: &Connection,
    engine: &Engine,
    attempt_id: &str,
) -> Result<CommandCandidate> {
    let attempt = read_command_attempt(connection, attempt_id)?;
    if attempt.outcome != Some(ExecutionOutcome::Success) {
        return Err(Error::Invalid(format!(
            "command attempt {attempt_id} has not finished with success"
        )));
    }
    let record = verify_success(connection, attempt_id, &attempt.subject)?;
    // A cut-off stream must never become a smaller candidate.
    if record.output_truncated {
        return Err(Error::Invalid(
            "command output was truncated; candidate refused".into(),
        ));
    }
    let changes = CandidateOutput::parse_changes(&record.stdout)?;
    let base = &attempt.subject.revision;
    let revision = engine.checkpoint(Some(base), &changes)?;
    if engine.load_revision(&revision)?.tree == engine.load_revision(base)?.tree {
        return Err(Error::Invalid(
            "candidate does not change the base revision".into(),
        ));
    }
    Ok(CommandCandidate {
        attempt_id: attempt.attempt_id,
        record_digest: record.digest()?,
        base_revision: base.clone(),
        revision,
        changes: changes.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<FileChange>> {
        CandidateOutput::parse_changes(text.as_bytes())
    }

    fn change(path: &str, hex: &str) -> String {
        format!(r#"{{"version":1,"changes":[{{"path":"{path}","hex":{hex}}}]}}"#)
    }

    #[test]
    fn accepts_writes_and_deletes() {
        let changes =
            parse(" {\"version\":1,\"changes\":[{\"path\":\"a/b.txt\",\"hex\":\"68690a\"},{\"path\":\"c\",\"hex\":null}]}\n")
                .unwrap();
        assert_eq!(changes[0].content.as_deref(), Some(&b"hi\n"[..]));
        assert!(changes[1].content.is_none());
        assert_eq!(
            parse(&change("a.txt", "\"\"")).unwrap()[0].content,
            Some(vec![])
        );
    }

    #[test]
    fn rejects_malformed_protocol() {
        for text in [
            change("a", "\"AB\""),
            change("a", "\"abc\""),
            change("a", "\"zz\""),
            r#"{"version":1,"changes":[{"path":"a"}]}"#.into(),
            r#"{"version":1,"changes":[{"path":"a","hex":null},{"path":"a","hex":"00"}]}"#.into(),
            r#"{"version":1,"changes":[],"x":1}"#.into(),
            r#"{"version":1,"changes":[{"path":"a","hex":null,"mode":1}]}"#.into(),
            r#"{"version":2,"changes":[{"path":"a","hex":null}]}"#.into(),
            format!("{} trailing", change("a", "null")),
            format!("{}{}", change("a", "null"), change("b", "null")),
            r#"{"version":1,"changes":[]}"#.into(),
            "fake candidate output\n".into(),
        ] {
            assert!(parse(&text).is_err(), "{text}");
        }
    }

    #[test]
    fn rejects_unsafe_paths() {
        for path in [
            "/etc/x",
            "../x",
            "a/../x",
            "./x",
            "a/./x",
            "a//b",
            "a/",
            "",
            "a\\\\b",
            "a\\u0000b",
            ".git/config",
            "a/.GIT/x",
            "a/.Git",
            ".git",
            ".agentscm/x",
            "a/.AgentSCM-Workspace/state.sqlite-wal",
            "AGENTSCM-DATA/x",
        ] {
            assert!(parse(&change(path, "null")).is_err(), "{path}");
        }
        assert!(parse(&change(".gitignore", "null")).is_ok());
    }
}
