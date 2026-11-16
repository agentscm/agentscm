//! Delivery facts for existing sources; these records confer no task or edit authority.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::intent::text;
use super::work_reconciliation::WorkDeclarationsView;
use super::{HubError, Result};

pub const PROTOCOL: u32 = 1;
pub const MAX_PAGE: usize = 100;
pub const ATTEMPT_SECS: i64 = 30;
pub const MAX_ATTEMPTS: usize = 8;
pub const FRAME: &str = "AgentSCM source notice";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    HumanMessage,
    Coordination,
    Dependency,
    Operational,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NoticeSource {
    pub kind: SourceKind,
    pub id: String,
    pub version: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NoticeRecipient {
    pub token: String,
    pub session: String,
    pub work_id: Option<String>,
    pub generation: Option<i64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AttemptOutcome {
    NotAttempted,
    ProvenNoDispatch,
    PossiblyEnqueued,
    Unknown,
    TransportReported,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NoticeAttempt {
    pub id: String,
    pub incarnation: String,
    pub generation: Option<i64>,
    pub outcome: AttemptOutcome,
    pub created_at: i64,
    pub reported_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NoticeReceipt {
    pub id: String,
    pub source: NoticeSource,
    pub recipient: NoticeRecipient,
    pub created_at: i64,
    pub deadline_at: i64,
    pub offered_at: Option<i64>,
    pub read_at: Option<i64>,
    pub resolved_at: Option<i64>,
    pub withdrawn: bool,
    #[serde(default)]
    pub terminal_at: Option<i64>,
    pub attempts: Vec<NoticeAttempt>,
}

impl NoticeReceipt {
    pub fn new(
        repository: &str,
        source: NoticeSource,
        recipient: NoticeRecipient,
        now: i64,
    ) -> Self {
        // Incarnation is deliberately absent: resume cannot mint another enqueue budget.
        let id = format!("ntc_{}", digest(&(repository, &source, &recipient)));
        Self {
            id,
            source,
            recipient,
            created_at: now,
            deadline_at: now.saturating_add(super::agent_messages::Reach::NextStep.pickup_secs()),
            offered_at: None,
            read_at: None,
            resolved_at: None,
            withdrawn: false,
            terminal_at: None,
            attempts: vec![],
        }
    }

    pub fn pending(&self) -> bool {
        !self.withdrawn && self.read_at.is_none() && self.resolved_at.is_none()
    }

    pub fn automatic_eligible(&self) -> bool {
        self.pending()
            && self.terminal_at.is_none()
            && self.attempts.len() < MAX_ATTEMPTS
            && self
                .attempts
                .iter()
                .all(|attempt| matches!(attempt.outcome, AttemptOutcome::ProvenNoDispatch))
    }

    pub fn needs_recovery(&self, now: i64) -> bool {
        self.pending() && (self.terminal_at.is_some() || now >= self.deadline_at)
    }

    pub fn claim(&mut self, incarnation: &str, attempt_id: String, now: i64) -> bool {
        if !self.automatic_eligible()
            || self.attempts.last().is_some_and(|last| {
                now < last
                    .reported_at
                    .unwrap_or(last.created_at)
                    .saturating_add(ATTEMPT_SECS)
            })
        {
            return false;
        }
        self.attempts.push(NoticeAttempt {
            id: attempt_id,
            incarnation: incarnation.into(),
            generation: self.recipient.generation,
            // Claim may reach a native enqueue before its reply is persisted locally.
            outcome: AttemptOutcome::PossiblyEnqueued,
            created_at: now,
            reported_at: None,
        });
        self.offered_at.get_or_insert(now);
        true
    }

    pub fn report(&mut self, report: &NoticeTransportReport) -> Result<()> {
        let attempt = self
            .attempts
            .iter_mut()
            .find(|attempt| {
                attempt.id == report.attempt_id && attempt.incarnation == report.incarnation
            })
            .ok_or_else(|| HubError::invalid_or_changed("Notice attempt changed.".into()))?;
        if attempt.reported_at.is_some() {
            return if attempt.outcome == report.outcome {
                Ok(())
            } else {
                Err(HubError::invalid_or_changed(
                    "Notice attempt already has another outcome.".into(),
                ))
            };
        }
        attempt.outcome = report.outcome;
        attempt.reported_at = Some(report.observed_at);
        Ok(())
    }

    pub fn pointer(&self) -> String {
        let read = match self.source.kind {
            SourceKind::Dependency => "agentscm_work_dependency_notices",
            SourceKind::Coordination => "agentscm_coordination_inbox",
            SourceKind::HumanMessage => "agentscm_hub_message",
            SourceKind::Operational => "agentscm_work_recovery",
        };
        let acknowledge = match self.source.kind {
            SourceKind::Dependency => {
                " After reviewing the current mailbox record, call agentscm_work_dependency_ack with this delivery_id and its current work_id, generation, declaration_seq and semantic_version. Reading or transport delivery alone is not acknowledgement."
            }
            SourceKind::HumanMessage => {
                " Call agentscm_hub_message with the SOURCE message id and status accepted before acting; obey its returned current consent mode."
            }
            SourceKind::Coordination => {
                " Acknowledge the current directed request or reply with agentscm_coordination_ack and its current sequence; acknowledgement grants no edit consent."
            }
            SourceKind::Operational => {
                " Use the permitted recovery preview; this pointer grants no takeover or resume authority."
            }
        };
        format!(
            "{FRAME} {}: {:?} source {} version {}. Read {read} for the current source. A transport offer is not acknowledgement or permission. Revalidate the current Work generation before acting.{acknowledge}",
            self.id, self.source.kind, self.source.id, self.source.version
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyRefresh {
    pub session: String,
    pub incarnation: String,
    pub after: Option<String>,
    pub limit: usize,
}

impl DependencyRefresh {
    pub fn validate(&self) -> Result<()> {
        text("session", &self.session, 255)?;
        text("incarnation", &self.incarnation, 255)?;
        if let Some(after) = &self.after {
            super::work::validate_id("after", after, "wrk")?;
        }
        if !(1..=MAX_PAGE).contains(&self.limit) {
            return Err(HubError::Invalid(
                "Notice refresh limit must be 1-100.".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeRefreshPage {
    pub next_after: Option<String>,
    pub complete: bool,
    pub coverage_complete: bool,
    pub refreshed: usize,
    pub notices: Vec<NoticeReceipt>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeOfferRequest {
    pub session: String,
    pub incarnation: String,
    /// False offers a pointer on a call already made; true claims native enqueue budget.
    pub automatic: bool,
}

impl NoticeOfferRequest {
    pub fn validate(&self) -> Result<()> {
        text("session", &self.session, 255)?;
        text("incarnation", &self.incarnation, 255)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeOffer {
    pub delivery_id: String,
    pub attempt_id: Option<String>,
    pub incarnation: String,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeTransportReport {
    pub session: String,
    pub incarnation: String,
    pub delivery_id: String,
    pub attempt_id: String,
    pub outcome: AttemptOutcome,
    pub observed_at: i64,
}

impl NoticeTransportReport {
    pub fn validate(&self) -> Result<()> {
        text("session", &self.session, 255)?;
        text("incarnation", &self.incarnation, 255)?;
        text("delivery_id", &self.delivery_id, 255)?;
        text("attempt_id", &self.attempt_id, 255)?;
        if self.observed_at < 0
            || matches!(
                self.outcome,
                AttemptOutcome::NotAttempted | AttemptOutcome::PossiblyEnqueued
            )
        {
            return Err(HubError::Invalid(
                "Report a final transport outcome and observation time.".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyNoticeAck {
    pub session: String,
    pub incarnation: String,
    pub delivery_id: String,
    pub work_id: String,
    pub generation: i64,
    pub declaration_seq: i64,
    pub semantic_version: i64,
}

impl DependencyNoticeAck {
    pub fn validate(&self) -> Result<()> {
        text("session", &self.session, 255)?;
        text("incarnation", &self.incarnation, 255)?;
        text("delivery_id", &self.delivery_id, 255)?;
        super::work::validate_id("work_id", &self.work_id, "wrk")?;
        if self.generation < 1 || self.declaration_seq < 0 || self.semantic_version < 1 {
            return Err(HubError::Invalid(
                "Dependency acknowledgement requires current fences.".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoticeAcknowledgement {
    pub newly_acknowledged: bool,
    pub receipt: NoticeReceipt,
    pub current: super::work_dependencies::DependencyNoticeRecord,
}

/// Only condition meaning changes its logical version; polling and heartbeat facts do not.
pub fn dependency_fingerprint(
    declared: &WorkDeclarationsView,
    conditions: &[super::work_reconciliation::ConditionDisposition],
) -> String {
    let mut rows = declared
        .declarations
        .prerequisites
        .iter()
        .map(|condition| {
            let disposition = conditions.iter().find(|value| value.id == condition.id);
            let mut limits = disposition
                .map(|value| value.limits.clone())
                .unwrap_or_default();
            limits.sort();
            limits.dedup();
            (
                condition.id.clone(),
                &condition.requirement,
                disposition.map(|value| value.state),
                disposition.is_none_or(|value| value.obstructing),
                limits,
            )
        })
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    digest(&(declared.generation, declared.declaration_seq, rows))
}

fn digest(value: &impl Serialize) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(value).expect("typed notice serializes"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice() -> NoticeReceipt {
        NoticeReceipt::new(
            "example/repo",
            NoticeSource {
                kind: SourceKind::Dependency,
                id: "wrk_example".into(),
                version: "2".into(),
            },
            NoticeRecipient {
                token: "token".into(),
                session: "native".into(),
                work_id: Some("wrk_example".into()),
                generation: Some(1),
            },
            100,
        )
    }

    #[test]
    fn only_proven_no_dispatch_can_reclaim_native_enqueue() {
        let mut receipt = notice();
        assert_eq!(receipt.deadline_at, 700);
        assert!(receipt.claim("inc1", "attempt1".into(), 101));
        assert!(!receipt.claim("inc2", "attempt2".into(), 200));
        let mut report = NoticeTransportReport {
            session: "native".into(),
            incarnation: "inc1".into(),
            delivery_id: receipt.id.clone(),
            attempt_id: "attempt1".into(),
            outcome: AttemptOutcome::Unknown,
            observed_at: 102,
        };
        receipt.report(&report).unwrap();
        receipt.report(&report).unwrap();
        assert!(!receipt.automatic_eligible());
        report.outcome = AttemptOutcome::ProvenNoDispatch;
        assert!(receipt.report(&report).is_err());
        let mut known = notice();
        assert!(known.claim("inc1", "attempt1".into(), 101));
        report.outcome = AttemptOutcome::ProvenNoDispatch;
        known.report(&report).unwrap();
        assert!(!known.claim("inc2", "attempt2".into(), 120));
        assert!(known.claim("inc2", "attempt2".into(), 200));
        assert_eq!(known.deadline_at, 700);
    }
}
