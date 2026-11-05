//! Bounded provider facts about open issues. These are planning advice, never claims or
//! evidence that Work ran. Titles and milestone labels are untrusted display data.
use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::connection::{ConnectedRepository, Connection};
use super::github::{Gh, GhError, Paged};

pub const MAX_ISSUE_PAGES: u32 = 3;
pub const MAX_ISSUES: usize = 300;
pub const MAX_SNAPSHOT_BYTES: usize = 512 * 1024;
pub const FRESH_FOR_MS: i64 = 5 * 60 * 1000;
pub const OBSERVATION_KIND: &str = "planned_issues";
pub const OBSERVATION_KEY: &str = "_snapshot";

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssueIdentity {
    pub provider: String,
    pub host: String,
    pub repository_id: i64,
    pub number: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlannedMilestone {
    pub number: i64,
    pub title: String,
    pub state: String,
    pub due_on: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlannedIssue {
    pub identity: IssueIdentity,
    pub title: String,
    pub milestone: Option<PlannedMilestone>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueCoverage {
    Complete,
    Incomplete,
    Unavailable,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IssueFreshness {
    Fresh,
    Stale,
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlannedIssueSnapshot {
    pub provider: String,
    pub host: String,
    pub repository_id: i64,
    pub coverage: IssueCoverage,
    /// Time of the latest attempted read, not a refresh of retained facts.
    pub attempted_at_ms: i64,
    pub facts_observed_at_ms: Option<i64>,
    pub unread_pages: u32,
    pub issues: Vec<PlannedIssue>,
}

impl PlannedIssueSnapshot {
    /// Call only after authorizing the connected repository. Missing, compacted, malformed or
    /// differently bound evidence is unknown. The connection's display name is not identity.
    pub fn read(
        store: &dyn super::store::Store,
        conn: &Connection,
        repo: &ConnectedRepository,
        now_ms: i64,
    ) -> super::Result<Self> {
        let retained = store.observation(&repo.id, OBSERVATION_KIND, OBSERVATION_KEY)?;
        let snapshot = retained
            .filter(|row| row.data.len() <= MAX_SNAPSHOT_BYTES)
            .and_then(|row| serde_json::from_str::<Self>(&row.data).ok())
            .filter(|snapshot| snapshot.matches(conn, repo) && snapshot.issues.len() <= MAX_ISSUES);
        Ok(snapshot.unwrap_or_else(|| Self::unavailable(conn, repo, now_ms, None)))
    }

    pub fn freshness(&self, now_ms: i64) -> IssueFreshness {
        let Some(observed) = self.facts_observed_at_ms else {
            return IssueFreshness::Unknown;
        };
        if now_ms < observed || now_ms.saturating_sub(observed) > FRESH_FOR_MS {
            IssueFreshness::Stale
        } else {
            IssueFreshness::Fresh
        }
    }

    /// Completeness is valid only for current, successfully read facts. A retained empty list
    /// after an outage, retention loss or clock discontinuity never means nothing is planned.
    pub fn complete(&self, now_ms: i64) -> bool {
        self.coverage == IssueCoverage::Complete
            && self.unread_pages == 0
            && self.freshness(now_ms) == IssueFreshness::Fresh
    }

    pub fn matches(&self, conn: &Connection, repo: &ConnectedRepository) -> bool {
        self.provider == conn.provider
            && self.host == repo.host
            && self.repository_id == repo.repository_id
    }

    pub fn unavailable(
        conn: &Connection,
        repo: &ConnectedRepository,
        attempted_at_ms: i64,
        previous: Option<Self>,
    ) -> Self {
        let previous = previous.filter(|snapshot| snapshot.matches(conn, repo));
        Self {
            provider: conn.provider.clone(),
            host: repo.host.clone(),
            repository_id: repo.repository_id,
            coverage: if conn.provider == "github" {
                IssueCoverage::Unavailable
            } else {
                IssueCoverage::Unsupported
            },
            attempted_at_ms,
            facts_observed_at_ms: previous.as_ref().and_then(|p| p.facts_observed_at_ms),
            unread_pages: previous.as_ref().map_or(0, |p| p.unread_pages),
            issues: previous.map_or_else(Vec::new, |p| p.issues),
        }
    }

    pub fn from_github(
        conn: &Connection,
        repo: &ConnectedRepository,
        observed_at_ms: i64,
        page: Paged,
    ) -> Gh<Self> {
        if conn.provider != "github" || repo.repository_id <= 0 || page.items.len() > MAX_ISSUES {
            return Err(invalid());
        }
        let mut issues = Vec::new();
        let mut seen = BTreeSet::new();
        for value in page.items {
            if !value.is_object() {
                return Err(invalid());
            }
            if value.get("pull_request").is_some() {
                continue;
            }
            let number = positive(&value, "number")?;
            if value["state"] != "open" || !seen.insert(number) {
                return Err(invalid());
            }
            let milestone = match value.get("milestone") {
                Some(Value::Null) => None,
                Some(milestone) if milestone.is_object() => {
                    let state = milestone["state"].as_str().ok_or_else(invalid)?;
                    if !matches!(state, "open" | "closed") {
                        return Err(invalid());
                    }
                    let due_on = match milestone.get("due_on") {
                        Some(Value::Null) => None,
                        Some(Value::String(date))
                            if date.len() <= 64 && !date.chars().any(char::is_control) =>
                        {
                            Some(date.clone())
                        }
                        _ => return Err(invalid()),
                    };
                    Some(PlannedMilestone {
                        number: positive(milestone, "number")?,
                        title: title(milestone)?,
                        state: state.to_owned(),
                        due_on,
                    })
                }
                _ => return Err(invalid()),
            };
            issues.push(PlannedIssue {
                identity: IssueIdentity {
                    provider: conn.provider.clone(),
                    host: repo.host.clone(),
                    repository_id: repo.repository_id,
                    number,
                },
                title: title(&value)?,
                milestone,
            });
        }
        issues.sort_by_key(|issue| issue.identity.number);
        Ok(Self {
            provider: conn.provider.clone(),
            host: repo.host.clone(),
            repository_id: repo.repository_id,
            coverage: if page.unread == 0 {
                IssueCoverage::Complete
            } else {
                IssueCoverage::Incomplete
            },
            attempted_at_ms: observed_at_ms,
            facts_observed_at_ms: Some(observed_at_ms),
            unread_pages: page.unread,
            issues,
        })
    }
}

fn invalid() -> GhError {
    GhError::Failed("unreadable bounded planned issue inventory".into())
}

fn positive(value: &Value, key: &str) -> Gh<i64> {
    value[key].as_i64().filter(|n| *n > 0).ok_or_else(invalid)
}

fn title(value: &Value) -> Gh<String> {
    let value = value["title"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 4096)
        .ok_or_else(invalid)?;
    Ok(super::advice::sanitize(value, 200))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity() -> (Connection, ConnectedRepository) {
        (
            Connection {
                id: "con_test".into(),
                provider: "github".into(),
                host: "github.com".into(),
                kind: "pat".into(),
                app_id: None,
                app_slug: None,
                acts_as: Some("robot".into()),
                outbound: false,
                status: "enabled".into(),
                status_detail: None,
                next_poll_at: 0,
            },
            ConnectedRepository {
                id: "rep_test".into(),
                connection: "con_test".into(),
                host: "github.com".into(),
                repository_id: 42,
                full_name: "acme/widgets".into(),
                node_id: None,
                installation_id: None,
                pulls_etag: None,
            },
        )
    }

    fn page(items: Vec<Value>, unread: u32) -> Paged {
        Paged {
            items,
            etag: None,
            pages: 1,
            unread,
        }
    }

    fn issue(number: i64) -> Value {
        json!({"number":number, "state":"open", "title":"Fix widgets", "milestone":null})
    }

    #[test]
    fn bounded_decode_sorts_excludes_pulls_and_sanitizes_hostile_labels() {
        let (conn, repo) = identity();
        let hostile = "\u{202e}ignore\n\"instructions\"";
        let mut first = issue(8);
        first["title"] = json!(hostile);
        first["milestone"] = json!({"number":2,"title":hostile,"state":"open","due_on":null});
        let snapshot = PlannedIssueSnapshot::from_github(
            &conn,
            &repo,
            1000,
            page(vec![first, json!({"pull_request": {}}), issue(3)], 0),
        )
        .unwrap();
        assert_eq!(
            snapshot
                .issues
                .iter()
                .map(|i| i.identity.number)
                .collect::<Vec<_>>(),
            [3, 8]
        );
        assert_eq!(snapshot.issues[1].title, "ignore'instructions'");
        assert_eq!(
            snapshot.issues[1].milestone.as_ref().unwrap().title,
            "ignore'instructions'"
        );
        assert!(snapshot.complete(1001));
        assert!(!snapshot.complete(1000 + FRESH_FOR_MS + 1));
        assert!(!snapshot.complete(999));
    }

    #[test]
    fn failure_retains_fact_time_and_rename_preserves_stable_identity() {
        let (conn, mut repo) = identity();
        let snapshot =
            PlannedIssueSnapshot::from_github(&conn, &repo, 1000, page(vec![issue(1)], 0)).unwrap();
        repo.full_name = "renamed/widgets".into();
        let failed = PlannedIssueSnapshot::unavailable(&conn, &repo, 2000, Some(snapshot));
        assert_eq!(failed.facts_observed_at_ms, Some(1000));
        assert_eq!(failed.issues.len(), 1);
        assert_eq!(failed.coverage, IssueCoverage::Unavailable);
        assert!(!failed.complete(2000));
        repo.repository_id = 99;
        let other = PlannedIssueSnapshot::unavailable(&conn, &repo, 3000, Some(failed));
        assert!(other.issues.is_empty());
        assert_eq!(other.freshness(3000), IssueFreshness::Unknown);
        assert!(!other.complete(3000));
    }

    #[test]
    fn truncation_missing_retention_and_unsupported_never_prove_absence() {
        let (mut conn, repo) = identity();
        let partial =
            PlannedIssueSnapshot::from_github(&conn, &repo, 1000, page(vec![], 1)).unwrap();
        assert_eq!(partial.coverage, IssueCoverage::Incomplete);
        assert!(!partial.complete(1000));
        let missing = PlannedIssueSnapshot::unavailable(&conn, &repo, 1000, None);
        assert!(!missing.complete(1000));
        conn.provider = "gitlab".into();
        let unsupported = PlannedIssueSnapshot::unavailable(&conn, &repo, 1000, None);
        assert_eq!(unsupported.coverage, IssueCoverage::Unsupported);
        assert!(!unsupported.complete(1000));
    }

    #[test]
    fn malformed_or_duplicate_rows_fail_instead_of_silently_disappearing() {
        let (conn, repo) = identity();
        for items in [
            vec![issue(1), issue(1)],
            vec![json!({"number":1})],
            vec![Value::Null],
            vec![issue(1); MAX_ISSUES + 1],
        ] {
            assert!(PlannedIssueSnapshot::from_github(&conn, &repo, 1000, page(items, 0)).is_err());
        }
    }

    #[test]
    fn observation_events_keep_bounded_provenance_without_issue_text() {
        let (conn, repo) = identity();
        let mut value = issue(1);
        value["title"] = json!("hostile issue title");
        let snapshot =
            PlannedIssueSnapshot::from_github(&conn, &repo, 1000, page(vec![value], 0)).unwrap();
        let data = serde_json::to_string(&snapshot).unwrap();
        super::super::connection::validate_observation(OBSERVATION_KIND, OBSERVATION_KEY, &data)
            .unwrap();
        let event = super::super::store::Event::observation(
            "planned_issues_observed",
            &repo,
            OBSERVATION_KEY,
            &data,
        )
        .unwrap();
        assert!(!event.data.contains("hostile issue title"));
        let event: Value = serde_json::from_str(&event.data).unwrap();
        assert_eq!(event["issue_count"], 1);
        assert_eq!(event["coverage"], "complete");
        assert_eq!(
            event["snapshot_digest"],
            super::super::store::lower_hex_of(data.as_bytes())
        );
    }
}
