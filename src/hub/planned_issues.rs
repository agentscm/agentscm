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
    /// Time of the latest attempted read, or zero when unsupported; never refreshes retained facts.
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
            .filter(|snapshot| snapshot.matches(conn, repo) && snapshot.valid_retained());
        // A local cache miss is not a provider attempt. Zero means no retained attempt.
        let _ = now_ms;
        Ok(snapshot.unwrap_or_else(|| Self::unavailable(conn, repo, 0, None)))
    }

    fn valid_retained(&self) -> bool {
        self.issues.len() <= MAX_ISSUES
            && self
                .issues
                .windows(2)
                .all(|w| w[0].identity.number < w[1].identity.number)
            && self.issues.iter().all(|i| {
                i.identity.provider == self.provider
                    && i.identity.host == self.host
                    && i.identity.repository_id == self.repository_id
                    && i.identity.number > 0
                    && safe_label(&i.title)
                    && i.milestone.as_ref().is_none_or(|m| {
                        m.number > 0
                            && safe_label(&m.title)
                            && matches!(m.state.as_str(), "open" | "closed")
                            && m.due_on.as_ref().is_none_or(|d| {
                                d.len() <= 64
                                    && !d
                                        .chars()
                                        .any(|c| c.is_control() || super::advice::invisible(c))
                            })
                    })
            })
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
            // Unsupported providers never attempt an issue read. Keep this stable across polls.
            attempted_at_ms: if conn.provider == "github" {
                attempted_at_ms
            } else {
                0
            },
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
        let snapshot = Self {
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
        };
        // Character limits do not bound UTF-8 bytes. Reject an oversized optional read before
        // storage so it leaves planning unknown without interrupting the coordination poll.
        if serde_json::to_vec(&snapshot).map_err(|_| invalid())?.len() > MAX_SNAPSHOT_BYTES {
            return Err(invalid());
        }
        Ok(snapshot)
    }
}

/// A durable link is caller metadata, including links on finished Work. It does not prove
/// ongoing ownership or execution. Issues appear in exactly one planning bucket.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LinkedPlannedIssue {
    pub issue: PlannedIssue,
    pub work_ids: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PlannedIssuesView {
    /// Freshness and completeness were evaluated at this read/capture time. Frozen briefing
    /// replays preserve this value rather than claiming a new provider observation.
    pub evaluated_at_ms: i64,
    pub coverage: IssueCoverage,
    pub freshness: IssueFreshness,
    pub attempted_at_ms: Option<i64>,
    pub facts_observed_at_ms: Option<i64>,
    pub unread_pages: u32,
    pub linkage_complete: bool,
    pub complete: bool,
    pub unclaimed: Vec<PlannedIssue>,
    pub linked: Vec<LinkedPlannedIssue>,
    /// No link was seen, but the bounded linkage inventory cannot prove absence.
    pub linkage_unknown: Vec<PlannedIssue>,
}

impl Default for PlannedIssuesView {
    fn default() -> Self {
        Self {
            evaluated_at_ms: 0,
            coverage: IssueCoverage::Unavailable,
            freshness: IssueFreshness::Unknown,
            attempted_at_ms: None,
            facts_observed_at_ms: None,
            unread_pages: 0,
            linkage_complete: false,
            complete: false,
            unclaimed: Vec::new(),
            linked: Vec::new(),
            linkage_unknown: Vec::new(),
        }
    }
}

pub(super) const MAX_LINKAGE_WORK: usize = 1000;
pub const MAX_CANDIDATES: usize = 20;

/// Build one projection from transaction-local provider and caller facts. The inventory is
/// independent of overview pagination and reservation/heartbeat state.
pub(super) fn project(
    who: &super::work::WorkWho,
    snapshot: Option<&str>,
    rows: Vec<(String, Option<String>, Option<String>)>,
    now: i64,
) -> PlannedIssuesView {
    let mut linkage_complete = rows.len() <= MAX_LINKAGE_WORK;
    let mut links = std::collections::BTreeMap::<i64, Vec<String>>::new();
    for (id, source_issue, link) in rows.into_iter().take(MAX_LINKAGE_WORK) {
        let decoded = link
            .as_deref()
            .and_then(|s| serde_json::from_str::<IssueIdentity>(s).ok())
            .filter(|issue| {
                issue.provider == who.owner.identity.provider
                    && issue.host == who.owner.identity.host
                    && Some(issue.repository_id) == who.repository_id
                    && issue.number > 0
            });
        if link.is_some() && decoded.is_none() {
            linkage_complete = false;
        }
        let number = decoded
            .map(|issue| issue.number)
            .or_else(|| source_issue.and_then(|n| n.parse::<i64>().ok()));
        if let Some(number) = number.filter(|n| *n > 0) {
            links.entry(number).or_default().push(id);
        }
    }
    if who.owner.identity.provider != "github" {
        return PlannedIssuesView {
            evaluated_at_ms: now.saturating_mul(1000),
            coverage: IssueCoverage::Unsupported,
            linkage_complete,
            ..Default::default()
        };
    }
    let snapshot = snapshot
        .filter(|s| s.len() <= MAX_SNAPSHOT_BYTES)
        .and_then(|s| serde_json::from_str::<PlannedIssueSnapshot>(s).ok())
        .filter(|s| {
            s.provider == who.owner.identity.provider
                && s.host == who.owner.identity.host
                && Some(s.repository_id) == who.repository_id
                && s.valid_retained()
        });
    let Some(snapshot) = snapshot else {
        return PlannedIssuesView {
            evaluated_at_ms: now.saturating_mul(1000),
            linkage_complete,
            ..Default::default()
        };
    };
    let now_ms = now.saturating_mul(1000);
    // The Store clock has whole-second precision. An observation within that same second
    // cannot establish a backward clock; retain its precise timestamp for the assessment.
    let freshness_at_ms = snapshot
        .facts_observed_at_ms
        .filter(|observed| observed.div_euclid(1000) == now)
        .unwrap_or(now_ms);
    let mut view = PlannedIssuesView {
        evaluated_at_ms: now_ms,
        coverage: snapshot.coverage,
        freshness: snapshot.freshness(freshness_at_ms),
        attempted_at_ms: (snapshot.attempted_at_ms > 0).then_some(snapshot.attempted_at_ms),
        facts_observed_at_ms: snapshot.facts_observed_at_ms,
        unread_pages: snapshot.unread_pages,
        linkage_complete,
        complete: snapshot.complete(freshness_at_ms) && linkage_complete,
        ..Default::default()
    };
    for issue in snapshot.issues {
        if let Some(work_ids) = links.remove(&issue.identity.number) {
            view.linked.push(LinkedPlannedIssue { issue, work_ids });
        } else if linkage_complete {
            view.unclaimed.push(issue);
        } else {
            view.linkage_unknown.push(issue);
        }
    }
    view
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssueCandidateAdvice {
    pub planning: PlannedIssuesView,
    pub candidates: Vec<PlannedIssue>,
    pub references_complete: bool,
    pub choice_provided: bool,
}

impl PlannedIssuesView {
    /// Exact references only. Each whitespace token is parsed once so a foreign URL or
    /// qualified reference cannot fall back to its embedded local #number.
    pub(super) fn candidates(
        self,
        start: &super::work::WorkStart,
        who: &super::work::WorkWho,
    ) -> IssueCandidateAdvice {
        let mut numbers = BTreeSet::new();
        if start.source.is_none() {
            for token in start.title.split_whitespace().chain(
                start
                    .next_action
                    .as_deref()
                    .unwrap_or("")
                    .split_whitespace(),
            ) {
                let token = token.trim_matches(|c: char| {
                    matches!(
                        c,
                        '(' | ')' | '[' | ']' | ',' | '.' | ';' | ':' | '"' | '\'' | '`'
                    )
                });
                let raw = if let Some(n) = token.strip_prefix('#') {
                    Some(n)
                } else if let Some((repository, n)) = token.split_once('#') {
                    repository
                        .eq_ignore_ascii_case(&who.repository)
                        .then_some(n)
                } else if let Ok(url) = reqwest::Url::parse(token) {
                    let prefix = format!("/{}/issues/", who.repository);
                    if url.scheme() == "https"
                        && url.host_str() == Some(who.owner.identity.host.as_str())
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.port().is_none()
                        && url.query().is_none()
                        && url.fragment().is_none()
                        && let Some(number) = url
                            .path()
                            .strip_prefix(&prefix)
                            .and_then(|n| n.parse::<i64>().ok())
                            .filter(|n| *n > 0)
                    {
                        numbers.insert(number);
                    }
                    None
                } else {
                    None
                };
                if let Some(n) = raw
                    .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|n| n.parse::<i64>().ok())
                    .filter(|n| *n > 0)
                {
                    numbers.insert(n);
                }
            }
        }
        let references_complete = numbers.len() <= MAX_CANDIDATES;
        let candidates = self
            .unclaimed
            .iter()
            .chain(self.linked.iter().map(|l| &l.issue))
            .chain(&self.linkage_unknown)
            .filter(|i| numbers.contains(&i.identity.number))
            .cloned()
            .collect::<Vec<_>>();
        let mut candidates = candidates;
        candidates.sort_by_key(|i| i.identity.number);
        candidates.truncate(MAX_CANDIDATES);
        IssueCandidateAdvice {
            planning: self,
            candidates,
            references_complete,
            choice_provided: start.issue_choice.as_ref().is_some_and(|choice| {
                !matches!(choice, super::work_issues::WorkIssueChoice::NotProvided {})
            }),
        }
    }
}

fn safe_label(value: &str) -> bool {
    // `clean`, not `sanitize`: a title stored before the Hub message rewrite must stay valid.
    !value.is_empty() && super::advice::clean(value, 200) == value
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
    fn whole_second_projection_keeps_a_current_millisecond_observation_fresh() {
        let (conn, repo) = identity();
        let snapshot =
            PlannedIssueSnapshot::from_github(&conn, &repo, 1001, page(vec![issue(1)], 0)).unwrap();
        let encoded = serde_json::to_string(&snapshot).unwrap();
        let view = project(&who(), Some(&encoded), Vec::new(), 1);
        assert_eq!(view.freshness, IssueFreshness::Fresh);
        assert!(view.complete);
        assert_eq!(view.facts_observed_at_ms, Some(1001));
        assert_eq!(view.evaluated_at_ms, 1000);
        let backwards = project(&who(), Some(&encoded), Vec::new(), 0);
        assert_eq!(backwards.freshness, IssueFreshness::Stale);
        assert!(!backwards.complete);
        let expired = project(&who(), Some(&encoded), Vec::new(), 302);
        assert_eq!(expired.freshness, IssueFreshness::Stale);
        assert!(!expired.complete);
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
        assert_eq!(unsupported.attempted_at_ms, 0);
        assert_eq!(unsupported.facts_observed_at_ms, None);
        assert_eq!(unsupported.freshness(1000), IssueFreshness::Unknown);
        assert!(unsupported.issues.is_empty());
        assert!(!unsupported.complete(1000));
        let repeated =
            PlannedIssueSnapshot::unavailable(&conn, &repo, 2000, Some(unsupported.clone()));
        assert_eq!(repeated, unsupported);
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
    fn multibyte_labels_cannot_exceed_the_optional_snapshot_byte_bound() {
        let (conn, repo) = identity();
        let label = "🧪".repeat(200);
        let items = (1..=i64::try_from(MAX_ISSUES).unwrap())
            .map(|number| {
                json!({"number":number,"state":"open","title":label,
                    "milestone":{"number":1,"title":label,"state":"open","due_on":null}})
            })
            .collect();
        assert!(PlannedIssueSnapshot::from_github(&conn, &repo, 1000, page(items, 0)).is_err());
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
    fn who() -> super::super::work::WorkWho {
        super::super::work::WorkWho {
            token: "token".into(),
            repository: "acme/widgets".into(),
            repository_id: Some(42),
            agent: "fixture".into(),
            read_only: false,
            owner: super::super::store::User {
                id: "owner".into(),
                identity: super::super::store::ForgeIdentity {
                    provider: "github".into(),
                    host: "github.com".into(),
                    user_id: 1,
                },
                login: "fixture".into(),
                name: None,
                disabled: false,
            },
        }
    }

    #[test]
    fn exact_candidates_never_reinterpret_foreign_references() {
        let (conn, repo) = identity();
        let snapshot = PlannedIssueSnapshot::from_github(
            &conn,
            &repo,
            1000,
            page((1..=5).map(issue).collect(), 0),
        )
        .unwrap();
        let raw = serde_json::to_string(&snapshot).unwrap();
        let view = project(&who(), Some(&raw), Vec::new(), 1);
        let start: super::super::work::WorkStart = serde_json::from_value(json!({"title":"#1 acme/widgets#2 https://github.com/acme/widgets/issues/3 other/repo#4 https://evil.example/acme/widgets/issues/5 https://evil.example/x#5", "session":"fixture", "request_key":"candidate"})).unwrap();
        let advice = view.candidates(&start, &who());
        assert_eq!(
            advice
                .candidates
                .iter()
                .map(|i| i.identity.number)
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(advice.references_complete);
        assert!(!advice.choice_provided);
    }

    #[test]
    fn missing_linkage_rows_and_corrupt_snapshots_never_prove_unclaimed() {
        let (conn, repo) = identity();
        let mut snapshot =
            PlannedIssueSnapshot::from_github(&conn, &repo, 1000, page(vec![issue(1)], 0)).unwrap();
        let rows = (0..=MAX_LINKAGE_WORK)
            .map(|n| (format!("work{n}"), None, None))
            .collect();
        let view = project(
            &who(),
            Some(&serde_json::to_string(&snapshot).unwrap()),
            rows,
            1,
        );
        assert!(!view.complete);
        assert!(view.unclaimed.is_empty());
        assert_eq!(view.linkage_unknown.len(), 1);
        snapshot.issues[0].identity.repository_id = 99;
        let corrupt = project(
            &who(),
            Some(&serde_json::to_string(&snapshot).unwrap()),
            Vec::new(),
            1,
        );
        assert!(!corrupt.complete);
        assert!(corrupt.unclaimed.is_empty());
        assert_eq!(corrupt.attempted_at_ms, None);
    }
}
