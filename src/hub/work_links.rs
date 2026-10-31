//! Explicit, caller-asserted Work to pull-request relationships.
//!
//! A link records a stable repository identity and a particular head. It does not
//! establish the provider's current head, verify execution, or grant approval.
use serde::{Deserialize, Serialize};

use super::work::{CheckpointReceipt, WorkItem, WorkWho, validate_id};
use super::{HubError, Result};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPrLinkTarget {
    pub repository_id: i64,
    pub number: i64,
    pub head_sha: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPrLinkUpdate {
    pub work_id: String,
    pub operation_id: String,
    pub generation: i64,
    pub session: String,
    pub expected_updated_seq: i64,
    /// None explicitly removes the single relationship; the JSON field is required.
    #[serde(deserialize_with = "explicit_link")]
    pub link: Option<WorkPrLinkTarget>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPrLinkQuery {
    pub expected_updated_seq: Option<i64>,
    /// A comparison with the recorded link, never a provider-head assertion.
    pub expected_head: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPrLink {
    pub target: WorkPrLinkTarget,
    pub author: String,
    pub token: String,
    pub agent: String,
    pub session: String,
    pub generation: i64,
    pub linked_at: i64,
    pub event_seq: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkPrSnapshotStatus {
    NotCompared,
    MatchesWork,
    WorkChanged,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkPrHeadStatus {
    NotCompared,
    Unlinked,
    MatchesLink,
    DiffersFromLink,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPrLinkView {
    pub work_id: String,
    pub repository: String,
    pub repository_id: i64,
    pub generation: i64,
    pub updated_seq: i64,
    pub link: Option<WorkPrLink>,
    pub snapshot_status: WorkPrSnapshotStatus,
    pub head_status: WorkPrHeadStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkPrLinkResult {
    pub view: WorkPrLinkView,
    /// The immutable transition receipt; the view may be newer on replay.
    pub receipt: CheckpointReceipt,
}

fn invalid(message: &str) -> HubError {
    HubError::Invalid(message.into())
}

fn label(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 255
        || value
            .chars()
            .any(|c| c.is_control() || super::advice::invisible(c))
    {
        return Err(invalid(
            "PR link operation_id and session must be nonempty labels of at most 255 bytes",
        ));
    }
    Ok(())
}

fn valid_head(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

impl WorkPrLinkQuery {
    pub(super) fn validate(&self) -> Result<()> {
        if self.expected_updated_seq.is_some_and(|seq| seq < 1)
            || self
                .expected_head
                .as_deref()
                .is_some_and(|head| !valid_head(head))
        {
            return Err(invalid(
                "PR link comparison requires a positive sequence and a full lowercase head SHA",
            ));
        }
        Ok(())
    }
}

impl WorkPrLinkUpdate {
    pub(super) fn validate(&self) -> Result<()> {
        validate_id("work", &self.work_id, "wrk")?;
        label(&self.operation_id)?;
        label(&self.session)?;
        if self.generation < 1 || self.expected_updated_seq < 1 {
            return Err(invalid(
                "PR link generation and expected_updated_seq must be positive",
            ));
        }
        if let Some(link) = &self.link
            && (link.repository_id < 1 || link.number < 1 || !valid_head(&link.head_sha))
        {
            return Err(invalid(
                "PR link requires positive repository_id and number and a full lowercase head SHA",
            ));
        }
        Ok(())
    }

    pub(super) fn authorize(&self, who: &WorkWho, item: &WorkItem) -> Result<i64> {
        let repository_id = trusted_repository_id(who)?;
        if who.read_only {
            return Err(HubError::Unauthorized);
        }
        if self
            .link
            .as_ref()
            .is_some_and(|link| link.repository_id != repository_id)
        {
            return Err(invalid(
                "PR link repository_id is outside the authorized Work repository",
            ));
        }
        if item.generation != self.generation {
            return Err(invalid("PR link generation is fenced"));
        }
        if item.lead_token != who.token || item.lead_session != self.session {
            return Err(invalid(
                "only the current Work owner and lead session may change a PR link",
            ));
        }
        Ok(repository_id)
    }
}

pub(super) fn trusted_repository_id(who: &WorkWho) -> Result<i64> {
    who.repository_id
        .filter(|id| *id > 0)
        .ok_or_else(|| invalid("PR relationships require a trusted stable repository ID"))
}

pub(super) fn view(
    item: &WorkItem,
    repository_id: i64,
    link: Option<WorkPrLink>,
    query: &WorkPrLinkQuery,
) -> WorkPrLinkView {
    let snapshot_status = match query.expected_updated_seq {
        None => WorkPrSnapshotStatus::NotCompared,
        Some(seq) if seq == item.updated_seq => WorkPrSnapshotStatus::MatchesWork,
        Some(_) => WorkPrSnapshotStatus::WorkChanged,
    };
    let head_status = match (&query.expected_head, &link) {
        (None, _) => WorkPrHeadStatus::NotCompared,
        (Some(_), None) => WorkPrHeadStatus::Unlinked,
        (Some(head), Some(link)) if *head == link.target.head_sha => WorkPrHeadStatus::MatchesLink,
        (Some(_), Some(_)) => WorkPrHeadStatus::DiffersFromLink,
    };
    WorkPrLinkView {
        work_id: item.id.clone(),
        repository: item.repository.clone(),
        repository_id,
        generation: item.generation,
        updated_seq: item.updated_seq,
        link,
        snapshot_status,
        head_status,
    }
}

pub(super) fn mutation_event(
    item: &WorkItem,
    who: &WorkWho,
    update: &WorkPrLinkUpdate,
    now: i64,
) -> Result<super::store::Event> {
    let mut event = super::store::Event::new(
        "work_pr_link",
        &item.id,
        serde_json::json!({
            "operation": if update.link.is_some() { "pr_link_set" } else { "pr_link_remove" },
            "generation": item.generation, "owner": item.owner,
            "repository": item.repository, "repository_id": who.repository_id,
            "relation": update.link, "author": who.owner.id, "token": who.token,
            "agent": who.agent, "session": update.session
        }),
    )?;
    event.at = now;
    Ok(event)
}

pub(super) fn asserted_link(
    who: &WorkWho,
    update: &WorkPrLinkUpdate,
    seq: i64,
    now: i64,
) -> Option<WorkPrLink> {
    update.link.clone().map(|target| WorkPrLink {
        target,
        author: who.owner.id.clone(),
        token: who.token.clone(),
        agent: who.agent.clone(),
        session: update.session.clone(),
        generation: update.generation,
        linked_at: now,
        event_seq: seq,
    })
}

fn explicit_link<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<WorkPrLinkTarget>, D::Error> {
    Option::<WorkPrLinkTarget>::deserialize(deserializer)
}

#[cfg(test)]
mod tests {
    use super::WorkPrLinkUpdate;

    #[test]
    fn removal_requires_explicit_null_link() {
        let mut value = serde_json::json!({
            "work_id": "wrk_00000000000000000000000000000001",
            "operation_id": "remove", "generation": 1, "session": "lead",
            "expected_updated_seq": 1
        });
        assert!(serde_json::from_value::<WorkPrLinkUpdate>(value.clone()).is_err());
        value["link"] = serde_json::Value::Null;
        assert!(
            serde_json::from_value::<WorkPrLinkUpdate>(value)
                .unwrap()
                .link
                .is_none()
        );
    }
}
