//! Native transport incarnation fences. A receipt never grants Work or process authority.
use serde::{Deserialize, Serialize};

use super::{HubError, Result};

/// Maximum unfinished led Work captured by one atomic terminal operation.
pub const MAX_LIFECYCLE_WORK: usize = 100;
/// An entry-captured legacy end may act only if the domain still has no lifecycle.
pub const EXPECT_NO_LIFECYCLE: &str = "unregistered";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Qualification {
    Unknown,
    Qualified,
    Mixed,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Live,
    Terminal,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LifecycleRegister {
    pub session: String,
    pub start_nonce: String,
    pub operation_id: String,
    pub observed_at: i64,
    pub expected_incarnation: Option<String>,
    pub qualified: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LifecycleView {
    pub session: String,
    pub incarnation: String,
    pub start_nonce: String,
    pub state: LifecycleState,
    pub qualification: Qualification,
    pub observed_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkFence {
    pub work_id: String,
    pub generation: i64,
    pub updated_seq: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    ObservedExit,
    ExplicitEnd,
    CoordinationStop,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LifecycleEnd {
    pub session: String,
    pub incarnation: String,
    pub operation_id: String,
    pub observed_at: i64,
    pub reason: EndReason,
    pub work: Vec<WorkFence>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LifecycleEndResult {
    pub lifecycle: LifecycleView,
    pub ended: bool,
    pub paused_work: Vec<String>,
}

pub(super) fn changed() -> HubError {
    HubError::invalid_or_changed(
        "session incarnation is invalid or changed; start_session to recover".into(),
    )
}

pub(super) fn encode(value: &impl Serialize) -> Result<String> {
    serde_json::to_string(value).map_err(|error| HubError::Unavailable(error.to_string()))
}

fn text(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > 128 || value.chars().any(char::is_control) {
        return Err(HubError::Invalid("invalid lifecycle identifier".into()));
    }
    Ok(())
}

fn observed(at: i64, now: i64) -> Result<()> {
    if at <= 0 || at > now.saturating_add(30) {
        return Err(HubError::Invalid(
            "invalid lifecycle observation time".into(),
        ));
    }
    Ok(())
}

impl LifecycleRegister {
    pub(super) fn validate(&self, now: i64) -> Result<()> {
        text(&self.session)?;
        text(&self.start_nonce)?;
        text(&self.operation_id)?;
        if let Some(expected) = &self.expected_incarnation {
            text(expected)?;
        }
        observed(self.observed_at, now)
    }
}

impl LifecycleEnd {
    pub(super) fn validate(&self, now: i64) -> Result<()> {
        text(&self.session)?;
        text(&self.incarnation)?;
        text(&self.operation_id)?;
        observed(self.observed_at, now)?;
        if self.work.len() > MAX_LIFECYCLE_WORK {
            return Err(HubError::Invalid("too many lifecycle Work fences".into()));
        }
        let mut seen = std::collections::BTreeSet::new();
        for fence in &self.work {
            super::work::validate_id("work_id", &fence.work_id, "wrk")?;
            if fence.generation <= 0 || fence.updated_seq <= 0 || !seen.insert(&fence.work_id) {
                return Err(changed());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_request_work_limit_is_bounded_above_legacy_five() {
        let mut request = LifecycleEnd {
            session: "native".into(),
            incarnation: "incarnation".into(),
            operation_id: "end".into(),
            observed_at: 10,
            reason: EndReason::ExplicitEnd,
            work: (0..MAX_LIFECYCLE_WORK)
                .map(|n| WorkFence {
                    work_id: format!("wrk_{n:032x}"),
                    generation: 1,
                    updated_seq: 1,
                })
                .collect(),
        };
        assert!(request.validate(10).is_ok());
        assert!(serde_json::to_vec(&request).unwrap().len() < 16 * 1024);
        request.work.push(WorkFence {
            work_id: format!("wrk_{:032x}", MAX_LIFECYCLE_WORK),
            generation: 1,
            updated_seq: 1,
        });
        assert!(request.validate(10).is_err());
    }

    #[test]
    fn replay_keeps_the_original_time_and_rejects_future_observations() {
        let mut request = LifecycleRegister {
            session: "native".into(),
            start_nonce: "start".into(),
            operation_id: "op".into(),
            observed_at: 10,
            expected_incarnation: None,
            qualified: false,
        };
        assert!(request.validate(100).is_ok());
        assert_eq!(request.observed_at, 10);
        request.observed_at = 131;
        assert!(request.validate(100).is_err());
    }
}
