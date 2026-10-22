//! Optional external CI and deployment outcomes seen for a landed revision.
//! Observations only: nothing here feeds checks, admission or publication updates.
use serde_json::json;

use super::connection::is_lower_hex;
use super::store::Event;
use super::{HubError, Result};

const MAX_TEXT_CHARS: usize = 200;
const MAX_URL_BYTES: usize = 2048;

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
pub fn supersedes(stored: &ExternalRun, incoming: &ExternalRun) -> bool {
    incoming != stored
        && incoming.provider_updated_at >= stored.provider_updated_at
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
        assert!(supersedes(&stored, &run(ExternalOutcome::Pending, 10)));
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
}
