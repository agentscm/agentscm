//! Why a commit or a path exists: the Work recorded for it and that Work's decisions.
//!
//! A commit resolves through a pull request: the merge or head commit of an observed merge, or
//! the head an agent recorded with a Work's pull-request link, then the Work linked to that pull
//! request. A path resolves to the Work whose declared scope covers it. Nothing is inferred from
//! branch names or text, so a commit or path with no recorded Work stays unknown.
use serde::{Deserialize, Serialize};

use super::viewer::Viewer;
use super::work::{Checkpoint, CheckpointKind, SourceRef, WorkItem, WorkState};
use super::{HubError, Result};

/// Work items one answer lists; `work_complete` says whether more exist.
pub const WHY_WORK: usize = 10;
/// Decisions listed per Work item, newest first.
pub const WHY_DECISIONS: usize = 20;
/// Decision checkpoints the Hub keeps per Work item for `why` and search.
pub const MAX_WORK_DECISIONS: i64 = 100;
/// Pull requests one commit can name.
pub const WHY_PULLS: usize = 10;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WhyQuery {
    /// A full lowercase commit SHA.
    pub commit: Option<String>,
    /// A repository-relative file, or a directory ending in `/` or `/**`.
    pub path: Option<String>,
}

/// The validated subject of a [`WhyQuery`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WhySubject {
    Commit(String),
    Path(String),
}

impl WhyQuery {
    pub fn subject(&self) -> Result<WhySubject> {
        match (&self.commit, &self.path) {
            (Some(commit), None)
                if matches!(commit.len(), 40 | 64)
                    && commit
                        .bytes()
                        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) =>
            {
                Ok(WhySubject::Commit(commit.clone()))
            }
            (None, Some(path)) => {
                let spec = path.strip_suffix('/').map(|dir| format!("{dir}/**"));
                Ok(WhySubject::Path(super::intent::spec_key(
                    spec.as_deref().unwrap_or(path),
                )?))
            }
            _ => Err(HubError::Invalid(
                "why needs exactly one of commit (a full lowercase SHA) or path".into(),
            )),
        }
    }
}

/// The JSON-encoded scope entries that cover `path`: itself and each parent directory. Stores
/// match them as whole elements of a Work's stored scope array.
pub(super) fn covering_scope_entries(path: &str) -> Vec<String> {
    let mut entries = vec![path.to_owned()];
    let trimmed = path.strip_suffix('/').unwrap_or(path);
    entries.extend(
        trimmed
            .match_indices('/')
            .map(|(end, _)| format!("{}/", &trimmed[..end])),
    );
    entries
        .iter()
        .map(|entry| serde_json::Value::String(entry.clone()).to_string())
        .collect()
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WhyPullRequest {
    pub number: i64,
    pub head_commit: Option<String>,
    /// Known only from a retained provider merge observation.
    pub merge_commit: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WhyRelation {
    /// The Work's explicit pull-request link names a pull request of the commit.
    PullRequest,
    /// The Work's declared scope covers the path.
    Scope,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WhyDecision {
    pub event_seq: i64,
    pub at: i64,
    pub summary: String,
    pub source_refs: Vec<SourceRef>,
    pub agent: String,
    pub session: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WhyOutcome {
    pub kind: CheckpointKind,
    pub summary: String,
    pub at: i64,
}

/// Where the Work's recorded agent conversation can be read. Those reads apply their own access
/// rules, which can leave them empty for this caller.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WhyConversation {
    pub change_record: String,
    pub conversation: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WhyWork {
    pub id: String,
    pub relation: WhyRelation,
    pub pull_number: Option<i64>,
    pub title: String,
    pub description: Option<String>,
    pub state: WorkState,
    pub owner_login: String,
    pub lead_agent: String,
    pub branch: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// The latest checkpoint the Work recorded.
    pub outcome: Option<WhyOutcome>,
    pub decisions: Vec<WhyDecision>,
    pub decisions_complete: bool,
    pub conversation: WhyConversation,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct WhyAnswer {
    pub commit: Option<String>,
    pub path: Option<String>,
    pub pull_requests: Vec<WhyPullRequest>,
    pub work: Vec<WhyWork>,
    pub work_complete: bool,
    /// False when the Hub has no Work recorded for the subject: unknown, not "no reason".
    pub known: bool,
}

/// What a store read for one listed Work item.
pub(super) struct WhyWorkFacts {
    pub item: WorkItem,
    pub relation: WhyRelation,
    pub pull_number: Option<i64>,
    /// `(event_seq, checkpoint)`, newest first, read with one past [`WHY_DECISIONS`].
    pub decisions: Vec<(i64, Checkpoint)>,
}

/// The listed Work as `viewer` may see it: another owner's agent labels and session ids are
/// masked as in every other Work read.
pub(super) fn work(facts: WhyWorkFacts, viewer: Viewer<'_>) -> WhyWork {
    let WhyWorkFacts {
        mut item,
        relation,
        pull_number,
        mut decisions,
    } = facts;
    let decisions_complete = decisions.len() <= WHY_DECISIONS;
    decisions.truncate(WHY_DECISIONS);
    for (_, decision) in &mut decisions {
        viewer.checkpoint(&item.participants, decision);
    }
    viewer.work(&mut item);
    let records = format!(
        "/v1/hub/repos/{}/change-records/{}",
        item.repository.replace('/', "%2F"),
        item.id
    );
    WhyWork {
        relation,
        pull_number,
        outcome: item.current.as_ref().map(|current| WhyOutcome {
            kind: current.kind,
            summary: current.summary.clone(),
            at: current.at,
        }),
        decisions: decisions
            .into_iter()
            .map(|(event_seq, decision)| WhyDecision {
                event_seq,
                at: decision.at,
                summary: decision.summary,
                source_refs: decision.source_refs,
                agent: decision.agent,
                session: decision.session,
            })
            .collect(),
        decisions_complete,
        conversation: WhyConversation {
            conversation: format!("{records}/conversation"),
            change_record: records,
        },
        id: item.id,
        title: item.title,
        description: item.description,
        state: item.state,
        owner_login: item.owner_login,
        lead_agent: item.lead_agent,
        branch: item.branch,
        created_at: item.created_at,
        updated_at: item.updated_at,
    }
}

/// `work` was read with one past [`WHY_WORK`].
pub(super) fn answer(
    subject: &WhySubject,
    pull_requests: Vec<WhyPullRequest>,
    mut work: Vec<WhyWork>,
) -> WhyAnswer {
    let work_complete = work.len() <= WHY_WORK;
    work.truncate(WHY_WORK);
    let (commit, path) = match subject {
        WhySubject::Commit(commit) => (Some(commit.clone()), None),
        WhySubject::Path(path) => (None, Some(path.clone())),
    };
    WhyAnswer {
        commit,
        path,
        pull_requests,
        known: !work.is_empty(),
        work,
        work_complete,
    }
}

/// Merges the pull requests a commit names, from merge observations and from Work links.
pub(super) fn add_pull(pulls: &mut Vec<WhyPullRequest>, pull: WhyPullRequest) {
    if let Some(known) = pulls.iter_mut().find(|known| known.number == pull.number) {
        known.head_commit = known.head_commit.take().or(pull.head_commit);
        known.merge_commit = known.merge_commit.take().or(pull.merge_commit);
    } else if pulls.len() < WHY_PULLS {
        pulls.push(pull);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_is_covered_by_itself_and_each_parent_directory() {
        assert_eq!(
            covering_scope_entries("src/hub/why.rs"),
            ["\"src/hub/why.rs\"", "\"src/\"", "\"src/hub/\""]
        );
        assert_eq!(
            covering_scope_entries("src/hub/"),
            ["\"src/hub/\"", "\"src/\""]
        );
        assert_eq!(covering_scope_entries("README.md"), ["\"README.md\""]);
    }

    #[test]
    fn a_query_names_exactly_one_valid_subject() {
        let commit = "a".repeat(40);
        let query = |commit: Option<&str>, path: Option<&str>| WhyQuery {
            commit: commit.map(str::to_owned),
            path: path.map(str::to_owned),
        };
        assert_eq!(
            query(Some(&commit), None).subject().unwrap(),
            WhySubject::Commit(commit.clone())
        );
        assert_eq!(
            query(None, Some("src/hub/")).subject().unwrap(),
            WhySubject::Path("src/hub/".into())
        );
        for bad in [
            query(None, None),
            query(Some(&commit), Some("a")),
            query(Some("ABC"), None),
            query(Some(&"A".repeat(40)), None),
            query(None, Some("../x")),
            query(None, Some("/etc/passwd")),
        ] {
            assert!(bad.subject().is_err(), "{bad:?}");
        }
    }
}
