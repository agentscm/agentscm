//! Caller-declared issue choices and links. These confer no provider or execution evidence.
use serde::{Deserialize, Serialize};

use super::planned_issues::IssueIdentity;
use super::work::{WorkItem, WorkWho, validate_id};
use super::{HubError, Result};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkIssueChoice {
    NotProvided {},
    Link { issue: IssueIdentity },
    NewIssue { title: String },
    NoneApplies { reason: String },
}

impl Default for WorkIssueChoice {
    fn default() -> Self {
        Self::NotProvided {}
    }
}

impl WorkIssueChoice {
    pub(super) fn validate(&self, who: &WorkWho) -> Result<()> {
        match self {
            Self::Link { issue } => validate_issue(issue, who),
            Self::NewIssue { title } => label(title, 500),
            Self::NoneApplies { reason } => label(reason, 500),
            Self::NotProvided {} => Ok(()),
        }
    }

    pub fn linked_issue(&self) -> Option<&IssueIdentity> {
        match self {
            Self::Link { issue } => Some(issue),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkIssueLinkUpdate {
    pub work_id: String,
    pub operation_id: String,
    pub generation: i64,
    pub session: String,
    pub expected_updated_seq: i64,
    /// Required explicit null removes the current caller-declared relationship.
    #[serde(deserialize_with = "explicit_link")]
    pub link: Option<IssueIdentity>,
}

impl WorkIssueLinkUpdate {
    pub(super) fn validate(&self) -> Result<()> {
        validate_id("work", &self.work_id, "wrk")?;
        label(&self.operation_id, 255)?;
        label(&self.session, 255)?;
        if self.generation < 1 || self.expected_updated_seq < 1 {
            return Err(invalid(
                "issue link generation and expected_updated_seq must be positive",
            ));
        }
        Ok(())
    }

    pub(super) fn authorize(&self, who: &WorkWho, item: &WorkItem) -> Result<()> {
        if !matches!(item.source, super::work::WorkSource::Native { .. }) {
            return Err(invalid("issue link updates apply only to native Work"));
        }
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        if let Some(issue) = &self.link {
            validate_issue(issue, who)?;
        }
        if item.generation != self.generation {
            return Err(invalid("issue link generation is fenced"));
        }
        if item.lead_token != who.token || item.lead_session != self.session {
            return Err(invalid(
                "only the current Work lead session may change an issue link",
            ));
        }
        Ok(())
    }
}

fn validate_issue(issue: &IssueIdentity, who: &WorkWho) -> Result<()> {
    if issue.number < 1
        || issue.repository_id < 1
        || Some(issue.repository_id) != who.repository_id
        || issue.provider != who.owner.identity.provider
        || issue.host != who.owner.identity.host
    {
        return Err(invalid(
            "issue link is outside the authorized stable Work repository",
        ));
    }
    Ok(())
}

fn label(value: &str, max: usize) -> Result<()> {
    if value.is_empty()
        || value.len() > max
        || value
            .chars()
            .any(|c| c.is_control() || super::advice::invisible(c))
    {
        return Err(invalid(
            "issue choice labels must be nonempty, bounded text without controls",
        ));
    }
    Ok(())
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.into())
}

fn explicit_link<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<IssueIdentity>, D::Error> {
    Option::<IssueIdentity>::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn removal_requires_explicit_null_and_choices_are_strict() {
        let mut value = serde_json::json!({"work_id":"wrk_00000000000000000000000000000001", "operation_id":"remove", "generation":1, "session":"lead", "expected_updated_seq":1});
        assert!(serde_json::from_value::<WorkIssueLinkUpdate>(value.clone()).is_err());
        value["link"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<WorkIssueLinkUpdate>(value)
                .unwrap()
                .link
                .is_none()
        );
        assert!(
            serde_json::from_value::<WorkIssueChoice>(serde_json::json!({"kind":"none_applies"}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<WorkIssueChoice>(
                serde_json::json!({"kind":"not_provided","extra":true})
            )
            .is_err()
        );
    }
}
