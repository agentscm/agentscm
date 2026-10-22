//! Optional external CI and deployment outcomes seen for a landed revision.
//! Observations only: nothing here feeds checks, admission or publication updates.
use serde::Deserialize;
use serde_json::{Value, json};

use super::HubState;
use super::connection::{ConnectedRepository, Connection, is_lower_hex};
use super::github::{Gh, GhError, parse_rfc3339};
use super::http::blocking;
use super::publish::{Update, merged_commit, publication_on_forge};
use super::store::{Event, unix_now};
use super::{HubError, Result};

const MAX_TEXT_CHARS: usize = 200;
const MAX_URL_BYTES: usize = 2048;
/// Publications read (and pull requests checked for a merge) per repository per pass.
const MAX_PER_PASS: usize = 20;
/// Deployment statuses read per commit; the rest are dropped and the read is incomplete.
const MAX_STATUSES: usize = 50;
/// A commit whose last read was complete and whose stored runs are all terminal is read again
/// only after this many seconds, so a new deployment, a rerun (new attempt) or a later status
/// change on it can appear up to this long (plus one poll interval) late.
const SETTLED_SECONDS: i64 = 3600;
/// Stored runs of one kind loaded to judge whether a commit has settled.
const SETTLED_ROWS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalKind {
    Ci,
    Deployment,
}

impl ExternalKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ci => "ci",
            Self::Deployment => "deployment",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "ci" => Some(Self::Ci),
            "deployment" => Some(Self::Deployment),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalOutcome {
    Pending,
    Running,
    Succeeded,
    Failed,
    Cancelled,
    /// A GitHub deployment that went `inactive`; never used for CI.
    Superseded,
    Unknown,
}

impl ExternalOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
            Self::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "cancelled" => Some(Self::Cancelled),
            "superseded" => Some(Self::Superseded),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Superseded
        )
    }
}

/// One attempt of a provider run, keyed by (repository, provider, kind, `run_id`, `attempt`).
/// Text fields come from the provider and are untrusted display data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalRun {
    /// Only `github` for now.
    pub provider: String,
    pub kind: ExternalKind,
    pub run_id: i64,
    pub attempt: i64,
    pub commit_sha: String,
    /// CI only.
    pub branch: Option<String>,
    /// Deployments only.
    pub environment: Option<String>,
    /// Workflow path or deployment description.
    pub name: String,
    pub outcome: ExternalOutcome,
    /// The provider's own update time, unix seconds.
    pub provider_updated_at: i64,
    pub html_url: Option<String>,
}

/// Column order read by both backends' row mappers.
pub(super) const EXTERNAL_RUN_COLUMNS: &str = "provider, kind, run_id, attempt, commit_sha, branch,
    environment, name, outcome, provider_updated_at, html_url";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExternalObserved {
    /// A newer or terminal stored state wins.
    Stale,
    Unchanged,
    Changed,
}

/// Whether `incoming` should replace `stored` for the same key. An identical run, one the
/// provider updated earlier, or a non-terminal state after a terminal one never replaces it.
/// At the same provider time only a terminal state replaces, so open states cannot flip-flop.
pub fn supersedes(stored: &ExternalRun, incoming: &ExternalRun) -> bool {
    let (then, now) = (stored.provider_updated_at, incoming.provider_updated_at);
    incoming != stored
        && (now > then || (now == then && incoming.outcome.is_terminal()))
        && (!stored.outcome.is_terminal() || incoming.outcome.is_terminal())
}

fn bounded(text: &str) -> bool {
    text.chars().count() <= MAX_TEXT_CHARS && !text.chars().any(char::is_control)
}

fn bounded_some(text: &Option<String>) -> bool {
    text.as_deref()
        .is_none_or(|text| !text.is_empty() && bounded(text))
}

impl ExternalRun {
    pub(super) fn validate(&self) -> Result<()> {
        let kind_ok = match self.kind {
            ExternalKind::Ci => self.environment.is_none(),
            ExternalKind::Deployment => self.branch.is_none(),
        };
        let url_ok = self.html_url.as_deref().is_none_or(|url| {
            url.starts_with("https://")
                && url.len() <= MAX_URL_BYTES
                && !url.chars().any(|c| c.is_control() || c.is_whitespace())
        });
        let outcome_ok =
            self.kind == ExternalKind::Deployment || self.outcome != ExternalOutcome::Superseded;
        if self.provider != "github"
            || self.run_id < 1
            || self.attempt < 1
            || self.provider_updated_at < 0
            || !is_lower_hex(&self.commit_sha, 40)
            || !kind_ok
            || !outcome_ok
            || !url_ok
            || !bounded(&self.name)
            || !bounded_some(&self.branch)
            || !bounded_some(&self.environment)
        {
            return Err(HubError::Invalid("external run is out of bounds".into()));
        }
        Ok(())
    }
}

// The wire shapes below are from the GitHub REST documentation (workflow runs, deployments
// and deployment statuses), not verified against a live GitHub.

#[derive(Deserialize)]
struct WireRepository {
    id: i64,
}

#[derive(Deserialize)]
struct WireRun {
    id: i64,
    run_attempt: Option<i64>,
    head_sha: String,
    head_branch: Option<String>,
    event: String,
    status: String,
    conclusion: Option<String>,
    path: String,
    html_url: Option<String>,
    updated_at: String,
    repository: WireRepository,
}

#[derive(Deserialize)]
struct WireDeployment {
    id: i64,
    sha: String,
    environment: String,
    description: Option<String>,
    updated_at: String,
}

#[derive(Deserialize)]
struct WireStatus {
    state: String,
    updated_at: String,
    log_url: Option<String>,
    target_url: Option<String>,
}

/// Keeps a link only when it is https on the connection's own web host.
fn on_host(url: Option<&str>, web: &str) -> Option<String> {
    let prefix = format!("{}/", web.trim_end_matches('/'));
    url.filter(|url| prefix.starts_with("https://") && url.starts_with(&prefix))
        .map(str::to_owned)
}

fn clipped(text: &str) -> String {
    text.chars().take(MAX_TEXT_CHARS).collect()
}

/// What one listed workflow run is to the commit being read.
#[derive(Debug, PartialEq, Eq)]
pub enum Ci {
    /// Another repository, commit, branch or event: not this commit's outcome.
    Other,
    /// It could not be decoded, or is out of bounds: the read is incomplete.
    Invalid,
    Run(ExternalRun),
}

/// A push-event workflow run for exactly `sha` on `branch` in repository `repo_id`. `web` is
/// the connection's web base.
pub fn ci_run(value: &Value, repo_id: i64, sha: &str, branch: &str, web: &str) -> Ci {
    let Ok(run) = WireRun::deserialize(value) else {
        return Ci::Invalid;
    };
    if run.repository.id != repo_id
        || run.head_sha != sha
        || run.head_branch.as_deref() != Some(branch)
        || run.event != "push"
    {
        return Ci::Other;
    }
    let outcome = match (run.status.as_str(), run.conclusion.as_deref()) {
        ("queued" | "waiting" | "requested" | "pending", _) => ExternalOutcome::Pending,
        ("in_progress", _) => ExternalOutcome::Running,
        ("completed", Some("success")) => ExternalOutcome::Succeeded,
        ("completed", Some("failure" | "timed_out" | "startup_failure")) => ExternalOutcome::Failed,
        ("completed", Some("cancelled")) => ExternalOutcome::Cancelled,
        _ => ExternalOutcome::Unknown,
    };
    let Some(provider_updated_at) = parse_rfc3339(&run.updated_at) else {
        return Ci::Invalid;
    };
    let run = ExternalRun {
        provider: "github".into(),
        kind: ExternalKind::Ci,
        run_id: run.id,
        attempt: run.run_attempt.unwrap_or(1),
        commit_sha: run.head_sha,
        branch: Some(branch.to_owned()),
        environment: None,
        name: clipped(&run.path),
        outcome,
        provider_updated_at,
        html_url: on_host(run.html_url.as_deref(), web),
    };
    match run.validate() {
        Ok(()) => Ci::Run(run),
        Err(_) => Ci::Invalid,
    }
}

/// A deployment of exactly `sha` with its newest status (`None` reads as pending); `None` when
/// it is for another commit, malformed or out of bounds.
pub fn deployment(
    value: &Value,
    status: Option<&Value>,
    sha: &str,
    web: &str,
) -> Option<ExternalRun> {
    let deployment = WireDeployment::deserialize(value).ok()?;
    let status = status.map(WireStatus::deserialize).transpose().ok()?;
    if deployment.sha != sha {
        return None;
    }
    let outcome = match status.as_ref().map(|status| status.state.as_str()) {
        None | Some("queued" | "pending") => ExternalOutcome::Pending,
        Some("in_progress") => ExternalOutcome::Running,
        Some("success") => ExternalOutcome::Succeeded,
        Some("failure" | "error") => ExternalOutcome::Failed,
        Some("inactive") => ExternalOutcome::Superseded,
        Some(_) => ExternalOutcome::Unknown,
    };
    let updated = |text: &str| parse_rfc3339(text);
    let provider_updated_at = match &status {
        Some(status) => updated(&status.updated_at)?.max(updated(&deployment.updated_at)?),
        None => updated(&deployment.updated_at)?,
    };
    let link = |status: &WireStatus| status.log_url.clone().or_else(|| status.target_url.clone());
    let run = ExternalRun {
        provider: "github".into(),
        kind: ExternalKind::Deployment,
        run_id: deployment.id,
        attempt: 1,
        commit_sha: deployment.sha,
        branch: None,
        environment: Some(deployment.environment),
        name: clipped(deployment.description.as_deref().unwrap_or_default()),
        outcome,
        provider_updated_at,
        html_url: on_host(status.as_ref().and_then(link).as_deref(), web),
    };
    run.validate().ok()?;
    Some(run)
}

/// Reads the merges and the external runs of one opted-in GitHub repository. Observations only:
/// the one publication change is recording a merge the forge reports, as a publication read
/// does. The caller holds the repository lock. `Auth` (a 401) and `Limited` stop the pass; a
/// 403 means the token may not read actions or deployments, so the repository is skipped
/// without blaming the connection. Any other failure skips one item and leaves its marker.
pub(super) async fn reconcile_repo(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
) -> Gh<()> {
    if conn.provider != "github" || !state.sync.external_runs_enabled(&repo.full_name) {
        return Ok(());
    }
    // The external reads turn a 403 into `Auth("403 ...")`, whether or not it carried rate-limit
    // headers; a 401 stays a failed credential.
    match pass(state, conn, repo, token).await {
        Err(GhError::Auth(detail)) if detail.starts_with("403") => {
            eprintln!("hub: external runs for {}: {detail}", repo.full_name);
            Ok(())
        }
        other => other,
    }
}

async fn pass(
    state: &HubState,
    conn: &Connection,
    repo: &ConnectedRepository,
    token: &str,
) -> Gh<()> {
    let oldest = unix_now() - state.sync.config()?.activity_days * 86_400;
    let (after_number, after_id) = state.sync.external_cursor(&repo.id);
    let (store, name) = (state.store.clone(), repo.full_name.clone());
    let unmerged = blocking(state, move || {
        store.pull_publications(&name, after_number, MAX_PER_PASS)
    })
    .await?;
    let full = unmerged.len() >= MAX_PER_PASS;
    let last = unmerged.last().and_then(|p| p.pr_number).unwrap_or(0);
    for publication in unmerged {
        let Some(number) = publication
            .pr_number
            .filter(|_| publication.updated_at >= oldest)
        else {
            continue;
        };
        if !publication_on_forge(state, &publication, repo).await {
            continue;
        }
        match merged_commit(state, conn, repo, number, &publication.commit).await {
            Ok(Some(merge_commit)) => {
                let (store, id) = (state.store.clone(), publication.id);
                blocking(state, move || {
                    store.update_publication(&id, &Update::Merged { merge_commit }, unix_now())?;
                    Ok(())
                })
                .await?;
            }
            Ok(None) => {}
            Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => return Err(error),
            Err(GhError::Failed(detail)) => {
                eprintln!(
                    "hub: external runs for {}#{number}: {detail}",
                    repo.full_name
                );
            }
        }
    }
    // Saved now so a stop in the run reads below keeps this progress; stage two keeps its own.
    state
        .sync
        .set_external_cursor(&repo.id, (if full { last } else { 0 }, after_id.clone()));

    let (store, name) = (state.store.clone(), repo.full_name.clone());
    let merged = blocking(state, move || {
        store.merged_publications(&name, &after_id, oldest, MAX_PER_PASS)
    })
    .await?;
    let next = match merged.last() {
        Some(p) if merged.len() >= MAX_PER_PASS => p.id.clone(),
        _ => String::new(),
    };
    for publication in merged {
        let Some(sha) = publication.merge_commit.as_deref() else {
            continue;
        };
        if !publication_on_forge(state, &publication, repo).await {
            continue;
        }
        if settled(state, &repo.id, sha).await? {
            continue;
        }
        match commit_runs(state, repo, token, sha, &publication.base).await {
            Ok(complete) => {
                let (store, id, sha) = (state.store.clone(), repo.id.clone(), sha.to_owned());
                blocking(state, move || {
                    store.mark_external_reconciled(&id, &sha, unix_now(), complete)
                })
                .await?;
            }
            Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => return Err(error),
            Err(GhError::Failed(detail)) => {
                eprintln!("hub: external runs for {} {sha}: {detail}", repo.full_name);
            }
        }
    }
    // Only after the page was read: a stop part-way reads the same page again next pass.
    state
        .sync
        .set_external_cursor(&repo.id, (if full { last } else { 0 }, next));
    Ok(())
}

/// Whether `sha` was read in full within [`SETTLED_SECONDS`] and every run stored for it is
/// terminal, so reading it again now would only spend requests. A commit with no stored run
/// is never settled.
async fn settled(state: &HubState, repo: &str, sha: &str) -> Result<bool> {
    let (store, repo, sha) = (state.store.clone(), repo.to_owned(), sha.to_owned());
    blocking(state, move || {
        let Some((at, true)) = store.external_reconciled(&repo, &sha)? else {
            return Ok(false);
        };
        if unix_now() - at >= SETTLED_SECONDS {
            return Ok(false);
        }
        let mut runs = Vec::new();
        for kind in [ExternalKind::Ci, ExternalKind::Deployment] {
            runs.extend(store.external_runs(&repo, &sha, kind, SETTLED_ROWS)?);
        }
        Ok(!runs.is_empty() && runs.iter().all(|run| run.outcome.is_terminal()))
    })
    .await
}

/// Reads and stores the runs and deployments of exactly `sha`. True when every list and every
/// item was read in full; dropped or unread items make it false rather than being guessed.
async fn commit_runs(
    state: &HubState,
    repo: &ConnectedRepository,
    token: &str,
    sha: &str,
    base: &str,
) -> Gh<bool> {
    let (api, web) = (state.sync.api()?, state.sync.config()?.web.as_str());
    let name = repo.full_name.as_str();
    let (runs, runs_done) = api.workflow_runs(token, name, sha).await?;
    let (deployments, deployments_done) = api.deployments(token, name, sha).await?;
    let mut found = Vec::new();
    let mut complete = runs_done && deployments_done;
    for run in &runs {
        match ci_run(run, repo.repository_id, sha, base, web) {
            Ci::Run(run) => found.push(run),
            Ci::Invalid => complete = false,
            Ci::Other => {}
        }
    }
    for (index, item) in deployments.iter().enumerate() {
        let Some(id) = item["id"].as_i64().filter(|_| index < MAX_STATUSES) else {
            complete = false;
            continue;
        };
        match api.deployment_status(token, name, id).await {
            Ok(status) => match deployment(item, status.as_ref(), sha, web) {
                Some(run) => found.push(run),
                None => complete = false,
            },
            Err(error @ (GhError::Auth(_) | GhError::Limited(_))) => return Err(error),
            Err(GhError::Failed(_)) => complete = false,
        }
    }
    // Runs of other branches, events or repositories are not this commit's outcome; anything
    // else dropped has already made the read incomplete.
    let dropped = runs.len() + deployments.len() - found.len();
    if dropped > 0 {
        eprintln!("hub: external runs for {name} {sha}: {dropped} items not matched");
    }
    for run in found {
        let (store, id) = (state.store.clone(), repo.id.clone());
        blocking(state, move || store.observe_external_run(&id, &run)).await?;
    }
    Ok(complete)
}

impl Event {
    /// Carries identifiers and the outcome only; the provider's text stays in the table.
    pub(super) fn external_run_observed(repository: &str, run: &ExternalRun) -> Result<Self> {
        let data = json!({"provider": run.provider, "kind": run.kind.as_str(),
            "run_id": run.run_id, "attempt": run.attempt, "commit_sha": run.commit_sha,
            "outcome": run.outcome.as_str(), "provider_updated_at": run.provider_updated_at});
        Self::new("external_run_observed", repository, data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(outcome: ExternalOutcome, at: i64) -> ExternalRun {
        ExternalRun {
            provider: "github".into(),
            kind: ExternalKind::Ci,
            run_id: 7,
            attempt: 1,
            commit_sha: "a".repeat(40),
            branch: Some("main".into()),
            environment: None,
            name: ".github/workflows/ci.yml".into(),
            outcome,
            provider_updated_at: at,
            html_url: Some("https://github.com/acme/widgets/actions/runs/7".into()),
        }
    }

    #[test]
    fn strings_roundtrip() {
        for kind in [ExternalKind::Ci, ExternalKind::Deployment] {
            assert_eq!(ExternalKind::parse(kind.as_str()), Some(kind));
        }
        for text in [
            "pending",
            "running",
            "succeeded",
            "failed",
            "cancelled",
            "superseded",
            "unknown",
        ] {
            assert_eq!(ExternalOutcome::parse(text).unwrap().as_str(), text);
        }
        assert_eq!(ExternalOutcome::parse("success"), None);
        assert_eq!(ExternalKind::parse("CI"), None);
    }

    #[test]
    fn newer_state_replaces_and_older_is_ignored() {
        let stored = run(ExternalOutcome::Running, 10);
        assert!(supersedes(&stored, &run(ExternalOutcome::Succeeded, 11)));
        assert!(supersedes(&stored, &run(ExternalOutcome::Pending, 11)));
        // Equal times: an open state does not replace another, a terminal one does.
        assert!(!supersedes(&stored, &run(ExternalOutcome::Pending, 10)));
        assert!(supersedes(&stored, &run(ExternalOutcome::Failed, 10)));
        assert!(!supersedes(&stored, &run(ExternalOutcome::Succeeded, 9)));
    }

    #[test]
    fn identical_observation_is_a_no_op() {
        let stored = run(ExternalOutcome::Failed, 10);
        assert!(!supersedes(&stored, &stored.clone()));
    }

    #[test]
    fn terminal_outcome_is_not_regressed() {
        for terminal in [
            ExternalOutcome::Succeeded,
            ExternalOutcome::Failed,
            ExternalOutcome::Cancelled,
            ExternalOutcome::Superseded,
        ] {
            for open in [
                ExternalOutcome::Pending,
                ExternalOutcome::Running,
                ExternalOutcome::Unknown,
            ] {
                assert!(!supersedes(&run(terminal, 10), &run(open, 20)));
            }
        }
        // A later terminal state, such as a deployment going inactive, does replace one.
        assert!(supersedes(
            &run(ExternalOutcome::Succeeded, 10),
            &run(ExternalOutcome::Failed, 11)
        ));
    }

    #[test]
    fn validation_accepts_a_good_run_and_rejects_bad_ones() {
        run(ExternalOutcome::Running, 1).validate().unwrap();
        let mut deploy = run(ExternalOutcome::Superseded, 1);
        deploy.kind = ExternalKind::Deployment;
        deploy.branch = None;
        deploy.environment = Some("production".into());
        deploy.validate().unwrap();

        let bad: &[fn(&mut ExternalRun)] = &[
            |r| r.provider = "gitlab".into(),
            |r| r.run_id = 0,
            |r| r.attempt = 0,
            |r| r.provider_updated_at = -1,
            |r| r.commit_sha = "A".repeat(40),
            |r| r.commit_sha = "a".repeat(39),
            |r| r.name = "x".repeat(201),
            |r| r.name = "ci\u{1b}[31m".into(),
            |r| r.branch = Some("a\nb".into()),
            |r| r.branch = Some(String::new()),
            |r| r.environment = Some("prod".into()),
            |r| r.html_url = Some("http://github.com/x".into()),
            |r| r.html_url = Some(format!("https://{}", "a".repeat(2048))),
            |r| r.html_url = Some("https://example.com/a b".into()),
            |r| r.outcome = ExternalOutcome::Superseded,
        ];
        for change in bad {
            let mut candidate = run(ExternalOutcome::Running, 1);
            change(&mut candidate);
            assert!(candidate.validate().is_err(), "{candidate:?}");
        }
    }

    const WEB: &str = "https://github.com";

    fn wire_run() -> Value {
        json!({"id": 9, "run_attempt": 2, "head_sha": "a".repeat(40), "head_branch": "main",
            "event": "push", "status": "completed", "conclusion": "success",
            "path": ".github/workflows/ci.yml", "updated_at": "2026-10-16T12:30:05Z",
            "html_url": "https://github.com/acme/widgets/actions/runs/9",
            "repository": {"id": 5}})
    }

    fn classify(value: &Value) -> Ci {
        ci_run(value, 5, &"a".repeat(40), "main", WEB)
    }

    fn ci(value: &Value) -> Option<ExternalRun> {
        match classify(value) {
            Ci::Run(run) => Some(run),
            _ => None,
        }
    }

    #[test]
    fn ci_run_maps_a_good_run() {
        let got = ci(&wire_run()).unwrap();
        assert_eq!(got.run_id, 9);
        assert_eq!(got.attempt, 2);
        assert_eq!(got.branch.as_deref(), Some("main"));
        assert_eq!(got.name, ".github/workflows/ci.yml");
        assert_eq!(got.provider_updated_at, 1_792_153_805);
        assert!(got.html_url.is_some());
    }

    #[test]
    fn ci_status_and_conclusion_map_to_outcomes() {
        let cases = [
            ("queued", None, ExternalOutcome::Pending),
            ("waiting", None, ExternalOutcome::Pending),
            ("requested", None, ExternalOutcome::Pending),
            ("pending", None, ExternalOutcome::Pending),
            ("in_progress", None, ExternalOutcome::Running),
            ("completed", Some("success"), ExternalOutcome::Succeeded),
            ("completed", Some("failure"), ExternalOutcome::Failed),
            ("completed", Some("timed_out"), ExternalOutcome::Failed),
            (
                "completed",
                Some("startup_failure"),
                ExternalOutcome::Failed,
            ),
            ("completed", Some("cancelled"), ExternalOutcome::Cancelled),
            ("completed", Some("skipped"), ExternalOutcome::Unknown),
            ("completed", Some("neutral"), ExternalOutcome::Unknown),
            (
                "completed",
                Some("action_required"),
                ExternalOutcome::Unknown,
            ),
            ("completed", Some("stale"), ExternalOutcome::Unknown),
            ("completed", Some("novel"), ExternalOutcome::Unknown),
            ("completed", None, ExternalOutcome::Unknown),
            ("novel", None, ExternalOutcome::Unknown),
        ];
        for (status, conclusion, want) in cases {
            let mut value = wire_run();
            value["status"] = status.into();
            value["conclusion"] = conclusion.map_or(Value::Null, Into::into);
            assert_eq!(ci(&value).unwrap().outcome, want, "{status} {conclusion:?}");
        }
    }

    #[test]
    fn ci_run_ignores_other_runs_but_flags_bad_data() {
        let edits: &[fn(&mut Value)] = &[
            |v| v["repository"]["id"] = 6.into(),
            |v| v["head_sha"] = "b".repeat(40).into(),
            |v| v["head_branch"] = "dev".into(),
            |v| v["head_branch"] = Value::Null,
            |v| v["event"] = "pull_request".into(),
        ];
        for edit in edits {
            let mut value = wire_run();
            edit(&mut value);
            assert_eq!(classify(&value), Ci::Other, "{value}");
        }
        let edits: &[fn(&mut Value)] = &[
            |v| v["id"] = "9".into(),
            |v| v["updated_at"] = "yesterday".into(),
            |v| v["path"] = "ci\u{1b}[31m".into(),
            |v| v["id"] = 0.into(),
            |v| {
                v.as_object_mut().unwrap().remove("repository");
            },
        ];
        for edit in edits {
            let mut value = wire_run();
            edit(&mut value);
            assert_eq!(classify(&value), Ci::Invalid, "{value}");
        }
    }

    #[test]
    fn ci_defaults_attempt_clips_name_and_filters_links() {
        let mut value = wire_run();
        value["run_attempt"] = Value::Null;
        value["path"] = "p".repeat(300).into();
        let got = ci(&value).unwrap();
        assert_eq!((got.attempt, got.name.len()), (1, 200));
        for url in [
            "http://github.com/a",
            "https://github.com.evil.test/a",
            "https://example.com/a",
            "https://github.com",
        ] {
            value["html_url"] = url.into();
            assert_eq!(ci(&value).unwrap().html_url, None, "{url}");
        }
        value["html_url"] = Value::Null;
        assert_eq!(ci(&value).unwrap().html_url, None);
        // A plain-http web host never yields a link.
        let got = ci_run(&wire_run(), 5, &"a".repeat(40), "main", "http://github.com");
        assert!(matches!(got, Ci::Run(run) if run.html_url.is_none()));
    }

    fn wire_deployment() -> Value {
        json!({"id": 3, "sha": "a".repeat(40), "ref": "main", "environment": "production",
            "description": "ship it", "updated_at": "2026-10-16T12:30:05Z"})
    }

    fn wire_status(state: &str) -> Value {
        json!({"id": 1, "state": state, "updated_at": "2026-10-16T12:31:05Z",
            "log_url": "https://github.com/acme/widgets/runs/1", "target_url": null})
    }

    fn deploy(value: &Value, status: Option<&Value>) -> Option<ExternalRun> {
        deployment(value, status, &"a".repeat(40), WEB)
    }

    #[test]
    fn deployment_status_maps_to_outcomes() {
        let cases = [
            ("queued", ExternalOutcome::Pending),
            ("pending", ExternalOutcome::Pending),
            ("in_progress", ExternalOutcome::Running),
            ("success", ExternalOutcome::Succeeded),
            ("failure", ExternalOutcome::Failed),
            ("error", ExternalOutcome::Failed),
            ("inactive", ExternalOutcome::Superseded),
            ("novel", ExternalOutcome::Unknown),
        ];
        for (state, want) in cases {
            let got = deploy(&wire_deployment(), Some(&wire_status(state))).unwrap();
            assert_eq!(got.outcome, want, "{state}");
            assert_eq!(got.provider_updated_at, 1_792_153_865);
            assert_eq!(got.environment.as_deref(), Some("production"));
            assert_eq!(
                (got.attempt, got.run_id, got.name.as_str()),
                (1, 3, "ship it")
            );
        }
        let got = deploy(&wire_deployment(), None).unwrap();
        assert_eq!(got.outcome, ExternalOutcome::Pending);
        assert_eq!(got.provider_updated_at, 1_792_153_805);
        assert_eq!(got.html_url, None);
    }

    #[test]
    fn deployment_link_prefers_log_url_then_target_url_on_host_only() {
        let mut status = wire_status("success");
        assert_eq!(
            deploy(&wire_deployment(), Some(&status))
                .unwrap()
                .html_url
                .as_deref(),
            Some("https://github.com/acme/widgets/runs/1")
        );
        status["log_url"] = Value::Null;
        status["target_url"] = "https://github.com/acme/widgets/deployments".into();
        assert!(
            deploy(&wire_deployment(), Some(&status))
                .unwrap()
                .html_url
                .is_some()
        );
        status["target_url"] = "https://example.com/x".into();
        assert_eq!(
            deploy(&wire_deployment(), Some(&status)).unwrap().html_url,
            None
        );
    }

    #[test]
    fn deployment_drops_other_commits_and_bad_data() {
        let edits: &[fn(&mut Value)] = &[
            |v| v["sha"] = "b".repeat(40).into(),
            |v| v["environment"] = "".into(),
            |v| v["environment"] = "e".repeat(201).into(),
            |v| v["environment"] = Value::Null,
            |v| v["updated_at"] = "soon".into(),
            |v| v["id"] = (-1).into(),
        ];
        for edit in edits {
            let mut value = wire_deployment();
            edit(&mut value);
            assert_eq!(deploy(&value, None), None, "{value}");
        }
        // A status that does not parse drops the deployment rather than guessing.
        assert_eq!(deploy(&wire_deployment(), Some(&json!({"state": 4}))), None);
        let mut value = wire_deployment();
        value["description"] = Value::Null;
        assert_eq!(deploy(&value, None).unwrap().name, "");
    }
}
