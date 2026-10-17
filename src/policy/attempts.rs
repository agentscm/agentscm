use crate::checks::CheckState;
use crate::{Error, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptView {
    pub state: CheckState,
    pub version: u64,
    pub run_generation: u64,
    pub lease_expires_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttemptFence {
    pub version: u64,
    pub run_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptAction {
    ClaimRun { lease_expires_at_ms: u64 },
    Complete { state: CheckState },
    Cancel,
    RecoverIndeterminate,
    RecoverExpired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttemptActionKind {
    ClaimRun,
    Complete,
    Cancel,
    RecoverIndeterminate,
    RecoverExpired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptTransitionOutcome {
    Planned(AttemptTransitionPlan),
    Refused(AttemptTransitionRefusal),
}

/// A pure structural transition description. It is not an owner lease,
/// dispatch capability, evidence qualification, authorization, or admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptTransitionPlan {
    pub action: AttemptActionKind,
    pub from_fence: AttemptFence,
    pub next_fence: AttemptFence,
    pub from_state: CheckState,
    pub next_state: CheckState,
    pub from_lease_expires_at_ms: Option<u64>,
    pub next_lease_expires_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttemptTransitionRefusal {
    StaleFence {
        expected: AttemptFence,
        actual: AttemptFence,
    },
    InvalidState {
        action: AttemptActionKind,
        state: CheckState,
    },
    UnknownClock {
        action: AttemptActionKind,
    },
    LeaseExpired {
        lease_expires_at_ms: u64,
        now_ms: u64,
    },
    LeaseNotExpired {
        lease_expires_at_ms: u64,
        now_ms: u64,
    },
    ExhaustedVersion {
        version: u64,
    },
    ExhaustedRunGeneration {
        run_generation: u64,
    },
}

impl AttemptView {
    pub fn validate(&self) -> Result<()> {
        validate_sql_integer("attempt version", self.version)?;
        validate_sql_integer("attempt run generation", self.run_generation)?;
        if let Some(lease_expires_at_ms) = self.lease_expires_at_ms {
            validate_positive_sql_integer("attempt lease expiry", lease_expires_at_ms)?;
        }

        match self.state {
            CheckState::Pending => {
                if self.version != 0 || self.run_generation != 0 {
                    return Err(Error::Invalid(
                        "pending attempt must have initial version and run generation zero"
                            .to_owned(),
                    ));
                }
                if self.lease_expires_at_ms.is_some() {
                    return Err(Error::Invalid(
                        "pending attempt must not have an active lease".to_owned(),
                    ));
                }
            }
            CheckState::Running => {
                if self.version == 0 || self.run_generation == 0 {
                    return Err(Error::Invalid(
                        "running attempt must have positive version and run generation".to_owned(),
                    ));
                }
                if self.lease_expires_at_ms.is_none() {
                    return Err(Error::Invalid(
                        "running attempt must have an active lease".to_owned(),
                    ));
                }
            }
            CheckState::Success
            | CheckState::Fail
            | CheckState::Incomplete
            | CheckState::Cancelled => {
                if self.version == 0 || self.run_generation == 0 {
                    return Err(Error::Invalid(
                        "terminal attempt must have positive version and run generation".to_owned(),
                    ));
                }
                if self.lease_expires_at_ms.is_some() {
                    return Err(Error::Invalid(
                        "terminal attempt must not have an active lease".to_owned(),
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn fence(&self) -> AttemptFence {
        AttemptFence {
            version: self.version,
            run_generation: self.run_generation,
        }
    }
}

impl AttemptFence {
    pub fn validate(&self) -> Result<()> {
        validate_sql_integer("expected attempt version", self.version)?;
        validate_sql_integer("expected attempt run generation", self.run_generation)
    }
}

impl AttemptAction {
    pub fn kind(&self) -> AttemptActionKind {
        match self {
            Self::ClaimRun { .. } => AttemptActionKind::ClaimRun,
            Self::Complete { .. } => AttemptActionKind::Complete,
            Self::Cancel => AttemptActionKind::Cancel,
            Self::RecoverIndeterminate => AttemptActionKind::RecoverIndeterminate,
            Self::RecoverExpired => AttemptActionKind::RecoverExpired,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::ClaimRun {
                lease_expires_at_ms,
            } => validate_positive_sql_integer("new attempt lease expiry", *lease_expires_at_ms),
            Self::Complete { state } if !is_terminal(*state) => Err(Error::Invalid(
                "attempt completion state must be terminal".to_owned(),
            )),
            Self::Complete { .. }
            | Self::Cancel
            | Self::RecoverIndeterminate
            | Self::RecoverExpired => Ok(()),
        }
    }
}

/// Plans one bounded lifecycle transition against an exact fence. The caller
/// must authenticate and re-read all durable authority state, then atomically
/// apply an unchanged plan inside its own transaction.
pub fn plan_attempt_transition(
    view: &AttemptView,
    expected_fence: AttemptFence,
    action: AttemptAction,
    now_ms: Option<u64>,
) -> Result<AttemptTransitionOutcome> {
    view.validate()?;
    expected_fence.validate()?;
    action.validate()?;
    if let Some(now_ms) = now_ms {
        validate_sql_integer("attempt transition time", now_ms)?;
    }

    let actual_fence = view.fence();
    if expected_fence != actual_fence {
        return Ok(AttemptTransitionOutcome::Refused(
            AttemptTransitionRefusal::StaleFence {
                expected: expected_fence,
                actual: actual_fence,
            },
        ));
    }

    let action_kind = action.kind();
    let (next_state, next_lease_expires_at_ms) = match action {
        AttemptAction::ClaimRun {
            lease_expires_at_ms,
        } => {
            if view.state != CheckState::Pending {
                return Ok(refused_invalid_state(action_kind, view.state));
            }
            let Some(now_ms) = now_ms else {
                return Ok(refused_unknown_clock(action_kind));
            };
            if lease_expires_at_ms <= now_ms {
                return Ok(AttemptTransitionOutcome::Refused(
                    AttemptTransitionRefusal::LeaseExpired {
                        lease_expires_at_ms,
                        now_ms,
                    },
                ));
            }
            (CheckState::Running, Some(lease_expires_at_ms))
        }
        AttemptAction::Complete { state } => {
            if view.state != CheckState::Running {
                return Ok(refused_invalid_state(action_kind, view.state));
            }
            let Some(now_ms) = now_ms else {
                return Ok(refused_unknown_clock(action_kind));
            };
            let lease_expires_at_ms = view.lease_expires_at_ms.ok_or_else(|| {
                Error::Invalid("running attempt must have an active lease".to_owned())
            })?;
            if now_ms >= lease_expires_at_ms {
                return Ok(AttemptTransitionOutcome::Refused(
                    AttemptTransitionRefusal::LeaseExpired {
                        lease_expires_at_ms,
                        now_ms,
                    },
                ));
            }
            (state, None)
        }
        AttemptAction::Cancel => {
            if !matches!(view.state, CheckState::Pending | CheckState::Running) {
                return Ok(refused_invalid_state(action_kind, view.state));
            }
            (CheckState::Cancelled, None)
        }
        AttemptAction::RecoverIndeterminate => {
            if view.state != CheckState::Running {
                return Ok(refused_invalid_state(action_kind, view.state));
            }
            (CheckState::Incomplete, None)
        }
        AttemptAction::RecoverExpired => {
            if view.state != CheckState::Running {
                return Ok(refused_invalid_state(action_kind, view.state));
            }
            let Some(now_ms) = now_ms else {
                return Ok(refused_unknown_clock(action_kind));
            };
            let lease_expires_at_ms = view.lease_expires_at_ms.ok_or_else(|| {
                Error::Invalid("running attempt must have an active lease".to_owned())
            })?;
            if now_ms < lease_expires_at_ms {
                return Ok(AttemptTransitionOutcome::Refused(
                    AttemptTransitionRefusal::LeaseNotExpired {
                        lease_expires_at_ms,
                        now_ms,
                    },
                ));
            }
            (CheckState::Incomplete, None)
        }
    };

    let Some(next_version) = view
        .version
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
    else {
        return Ok(AttemptTransitionOutcome::Refused(
            AttemptTransitionRefusal::ExhaustedVersion {
                version: view.version,
            },
        ));
    };
    // Completion publishes evidence for the claimed run. Cancellation/recovery
    // instead invalidate that run, even if no new dispatch will be attempted.
    let next_run_generation = if action_kind == AttemptActionKind::Complete {
        view.run_generation
    } else {
        let Some(next) = view
            .run_generation
            .checked_add(1)
            .filter(|value| *value <= i64::MAX as u64)
        else {
            return Ok(AttemptTransitionOutcome::Refused(
                AttemptTransitionRefusal::ExhaustedRunGeneration {
                    run_generation: view.run_generation,
                },
            ));
        };
        next
    };

    Ok(AttemptTransitionOutcome::Planned(AttemptTransitionPlan {
        action: action_kind,
        from_fence: actual_fence,
        next_fence: AttemptFence {
            version: next_version,
            run_generation: next_run_generation,
        },
        from_state: view.state,
        next_state,
        from_lease_expires_at_ms: view.lease_expires_at_ms,
        next_lease_expires_at_ms,
    }))
}

fn refused_invalid_state(action: AttemptActionKind, state: CheckState) -> AttemptTransitionOutcome {
    AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::InvalidState { action, state })
}

fn refused_unknown_clock(action: AttemptActionKind) -> AttemptTransitionOutcome {
    AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::UnknownClock { action })
}

fn is_terminal(state: CheckState) -> bool {
    matches!(
        state,
        CheckState::Success | CheckState::Fail | CheckState::Incomplete | CheckState::Cancelled
    )
}

fn validate_positive_sql_integer(kind: &str, value: u64) -> Result<()> {
    if value == 0 {
        return Err(Error::Invalid(format!("{kind} must be positive")));
    }
    validate_sql_integer(kind, value)
}

fn validate_sql_integer(kind: &str, value: u64) -> Result<()> {
    if value > i64::MAX as u64 {
        return Err(Error::Limit(format!("{kind} exceeds SQLite range")));
    }
    Ok(())
}
