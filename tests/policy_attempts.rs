use agentscm::Error;
use agentscm::checks::CheckState;
use agentscm::policy::{
    AttemptAction, AttemptActionKind, AttemptFence, AttemptTransitionOutcome,
    AttemptTransitionPlan, AttemptTransitionRefusal, AttemptView, plan_attempt_transition,
};

fn pending() -> AttemptView {
    AttemptView {
        state: CheckState::Pending,
        version: 0,
        run_generation: 0,
        lease_expires_at_ms: None,
    }
}

fn running(lease_expires_at_ms: u64) -> AttemptView {
    AttemptView {
        state: CheckState::Running,
        version: 1,
        run_generation: 1,
        lease_expires_at_ms: Some(lease_expires_at_ms),
    }
}

fn planned(outcome: AttemptTransitionOutcome) -> AttemptTransitionPlan {
    let AttemptTransitionOutcome::Planned(plan) = outcome else {
        panic!("expected a planned attempt transition")
    };
    plan
}

fn apply(plan: &AttemptTransitionPlan) -> AttemptView {
    AttemptView {
        state: plan.next_state,
        version: plan.next_fence.version,
        run_generation: plan.next_fence.run_generation,
        lease_expires_at_ms: plan.next_lease_expires_at_ms,
    }
}

#[test]
fn claim_advances_both_fences_and_completion_preserves_the_claimed_run() {
    let pending = pending();
    let claim = planned(
        plan_attempt_transition(
            &pending,
            pending.fence(),
            AttemptAction::ClaimRun {
                lease_expires_at_ms: 200,
            },
            Some(100),
        )
        .unwrap(),
    );
    assert_eq!(claim.action, AttemptActionKind::ClaimRun);
    assert_eq!(
        claim.from_fence,
        AttemptFence {
            version: 0,
            run_generation: 0
        }
    );
    assert_eq!(
        claim.next_fence,
        AttemptFence {
            version: 1,
            run_generation: 1
        }
    );
    assert_eq!(claim.from_state, CheckState::Pending);
    assert_eq!(claim.next_state, CheckState::Running);
    assert_eq!(claim.from_lease_expires_at_ms, None);
    assert_eq!(claim.next_lease_expires_at_ms, Some(200));

    let running = apply(&claim);
    let completion = planned(
        plan_attempt_transition(
            &running,
            running.fence(),
            AttemptAction::Complete {
                state: CheckState::Success,
            },
            Some(199),
        )
        .unwrap(),
    );
    assert_eq!(completion.action, AttemptActionKind::Complete);
    assert_eq!(
        completion.from_fence,
        AttemptFence {
            version: 1,
            run_generation: 1
        }
    );
    assert_eq!(
        completion.next_fence,
        AttemptFence {
            version: 2,
            run_generation: 1
        }
    );
    assert_eq!(completion.from_state, CheckState::Running);
    assert_eq!(completion.next_state, CheckState::Success);
    assert_eq!(completion.from_lease_expires_at_ms, Some(200));
    assert_eq!(completion.next_lease_expires_at_ms, None);
}

#[test]
fn mismatched_fence_is_a_typed_refusal() {
    let view = running(200);
    let outcome = plan_attempt_transition(
        &view,
        AttemptFence {
            version: 0,
            run_generation: 1,
        },
        AttemptAction::Cancel,
        None,
    )
    .unwrap();

    assert_eq!(
        outcome,
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::StaleFence {
            expected: AttemptFence {
                version: 0,
                run_generation: 1,
            },
            actual: view.fence(),
        })
    );
}

#[test]
fn cancellation_fences_a_late_finisher() {
    let running = running(200);
    let cancellation = planned(
        plan_attempt_transition(&running, running.fence(), AttemptAction::Cancel, None).unwrap(),
    );
    assert_eq!(cancellation.next_state, CheckState::Cancelled);
    assert_eq!(
        cancellation.next_fence,
        AttemptFence {
            version: 2,
            run_generation: 2
        }
    );
    assert_eq!(cancellation.next_lease_expires_at_ms, None);

    let cancelled = apply(&cancellation);
    let late = plan_attempt_transition(
        &cancelled,
        running.fence(),
        AttemptAction::Complete {
            state: CheckState::Success,
        },
        Some(150),
    )
    .unwrap();
    assert!(matches!(
        late,
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::StaleFence { .. })
    ));

    let current_fence_late = plan_attempt_transition(
        &cancelled,
        cancelled.fence(),
        AttemptAction::Complete {
            state: CheckState::Success,
        },
        Some(150),
    )
    .unwrap();
    assert_eq!(
        current_fence_late,
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::InvalidState {
            action: AttemptActionKind::Complete,
            state: CheckState::Cancelled,
        })
    );
}

#[test]
fn indeterminate_recovery_fences_a_late_finisher() {
    let running = running(200);
    let recovery = planned(
        plan_attempt_transition(
            &running,
            running.fence(),
            AttemptAction::RecoverIndeterminate,
            None,
        )
        .unwrap(),
    );
    assert_eq!(recovery.next_state, CheckState::Incomplete);
    assert_eq!(
        recovery.next_fence,
        AttemptFence {
            version: 2,
            run_generation: 2
        }
    );
    assert_eq!(recovery.next_lease_expires_at_ms, None);

    let recovered = apply(&recovery);
    let late = plan_attempt_transition(
        &recovered,
        running.fence(),
        AttemptAction::Complete {
            state: CheckState::Success,
        },
        Some(150),
    )
    .unwrap();
    assert!(matches!(
        late,
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::StaleFence { .. })
    ));
}

#[test]
fn uncertain_clock_blocks_start_completion_and_expiry_inference() {
    let pending = pending();
    assert_eq!(
        plan_attempt_transition(
            &pending,
            pending.fence(),
            AttemptAction::ClaimRun {
                lease_expires_at_ms: 200,
            },
            None,
        )
        .unwrap(),
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::UnknownClock {
            action: AttemptActionKind::ClaimRun,
        })
    );

    let running = running(200);
    assert_eq!(
        plan_attempt_transition(
            &running,
            running.fence(),
            AttemptAction::Complete {
                state: CheckState::Fail,
            },
            None,
        )
        .unwrap(),
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::UnknownClock {
            action: AttemptActionKind::Complete,
        })
    );
    assert_eq!(
        plan_attempt_transition(
            &running,
            running.fence(),
            AttemptAction::RecoverExpired,
            None,
        )
        .unwrap(),
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::UnknownClock {
            action: AttemptActionKind::RecoverExpired,
        })
    );
}

#[test]
fn uncertain_clock_still_allows_explicit_stop_actions() {
    let running = running(200);
    let cancellation = planned(
        plan_attempt_transition(&running, running.fence(), AttemptAction::Cancel, None).unwrap(),
    );
    assert_eq!(cancellation.next_state, CheckState::Cancelled);

    let recovery = planned(
        plan_attempt_transition(
            &running,
            running.fence(),
            AttemptAction::RecoverIndeterminate,
            None,
        )
        .unwrap(),
    );
    assert_eq!(recovery.next_state, CheckState::Incomplete);
}

#[test]
fn lease_expiry_boundary_is_exclusive_for_completion_and_inclusive_for_recovery() {
    let running = running(200);
    assert!(matches!(
        plan_attempt_transition(
            &running,
            running.fence(),
            AttemptAction::Complete {
                state: CheckState::Success,
            },
            Some(199),
        )
        .unwrap(),
        AttemptTransitionOutcome::Planned(_)
    ));
    assert_eq!(
        plan_attempt_transition(
            &running,
            running.fence(),
            AttemptAction::Complete {
                state: CheckState::Success,
            },
            Some(200),
        )
        .unwrap(),
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::LeaseExpired {
            lease_expires_at_ms: 200,
            now_ms: 200,
        })
    );
    assert_eq!(
        plan_attempt_transition(
            &running,
            running.fence(),
            AttemptAction::RecoverExpired,
            Some(199),
        )
        .unwrap(),
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::LeaseNotExpired {
            lease_expires_at_ms: 200,
            now_ms: 199,
        })
    );
    let recovered = planned(
        plan_attempt_transition(
            &running,
            running.fence(),
            AttemptAction::RecoverExpired,
            Some(200),
        )
        .unwrap(),
    );
    assert_eq!(recovered.next_state, CheckState::Incomplete);
    assert_eq!(recovered.next_lease_expires_at_ms, None);
}

#[test]
fn claim_requires_a_strictly_future_lease() {
    let pending = pending();
    assert_eq!(
        plan_attempt_transition(
            &pending,
            pending.fence(),
            AttemptAction::ClaimRun {
                lease_expires_at_ms: 100,
            },
            Some(100),
        )
        .unwrap(),
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::LeaseExpired {
            lease_expires_at_ms: 100,
            now_ms: 100,
        })
    );
}

#[test]
fn counter_exhaustion_is_typed_and_never_wraps() {
    let exhausted_version = AttemptView {
        state: CheckState::Running,
        version: i64::MAX as u64,
        run_generation: 1,
        lease_expires_at_ms: Some(200),
    };
    assert_eq!(
        plan_attempt_transition(
            &exhausted_version,
            exhausted_version.fence(),
            AttemptAction::Cancel,
            None,
        )
        .unwrap(),
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::ExhaustedVersion {
            version: i64::MAX as u64,
        })
    );

    let exhausted_generation = AttemptView {
        state: CheckState::Running,
        version: 1,
        run_generation: i64::MAX as u64,
        lease_expires_at_ms: Some(200),
    };
    let completion = planned(
        plan_attempt_transition(
            &exhausted_generation,
            exhausted_generation.fence(),
            AttemptAction::Complete {
                state: CheckState::Success,
            },
            Some(199),
        )
        .unwrap(),
    );
    assert_eq!(
        completion.next_fence,
        AttemptFence {
            version: 2,
            run_generation: i64::MAX as u64,
        }
    );
    for action in [
        AttemptAction::RecoverIndeterminate,
        AttemptAction::RecoverExpired,
    ] {
        assert_eq!(
            plan_attempt_transition(
                &exhausted_generation,
                exhausted_generation.fence(),
                action,
                Some(200)
            )
            .unwrap(),
            AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::ExhaustedRunGeneration {
                run_generation: i64::MAX as u64,
            }),
        );
    }
    assert_eq!(
        plan_attempt_transition(
            &exhausted_generation,
            exhausted_generation.fence(),
            AttemptAction::Cancel,
            None,
        )
        .unwrap(),
        AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::ExhaustedRunGeneration {
            run_generation: i64::MAX as u64,
        })
    );
}

#[test]
fn pending_cancel_advances_generation_without_creating_a_lease() {
    let pending = pending();
    let cancellation = planned(
        plan_attempt_transition(&pending, pending.fence(), AttemptAction::Cancel, None).unwrap(),
    );
    assert_eq!(cancellation.next_state, CheckState::Cancelled);
    assert_eq!(
        cancellation.next_fence,
        AttemptFence {
            version: 1,
            run_generation: 1
        }
    );
    assert_eq!(cancellation.next_lease_expires_at_ms, None);
}

#[test]
fn malformed_views_actions_and_bounds_return_errors() {
    let cases = [
        AttemptView {
            state: CheckState::Pending,
            version: 0,
            run_generation: 0,
            lease_expires_at_ms: Some(100),
        },
        AttemptView {
            state: CheckState::Pending,
            version: 1,
            run_generation: 1,
            lease_expires_at_ms: None,
        },
        AttemptView {
            state: CheckState::Running,
            version: 1,
            run_generation: 1,
            lease_expires_at_ms: None,
        },
        AttemptView {
            state: CheckState::Running,
            version: 1,
            run_generation: 0,
            lease_expires_at_ms: Some(100),
        },
        AttemptView {
            state: CheckState::Success,
            version: 1,
            run_generation: 1,
            lease_expires_at_ms: Some(100),
        },
        AttemptView {
            state: CheckState::Fail,
            version: 0,
            run_generation: 0,
            lease_expires_at_ms: None,
        },
    ];
    for view in cases {
        assert!(matches!(view.validate(), Err(Error::Invalid(_))));
    }

    let pending = pending();
    assert!(matches!(
        plan_attempt_transition(
            &pending,
            pending.fence(),
            AttemptAction::ClaimRun {
                lease_expires_at_ms: 0,
            },
            Some(0),
        ),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        plan_attempt_transition(
            &pending,
            AttemptFence {
                version: i64::MAX as u64 + 1,
                run_generation: 0,
            },
            AttemptAction::Cancel,
            None,
        ),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        plan_attempt_transition(
            &pending,
            pending.fence(),
            AttemptAction::Cancel,
            Some(i64::MAX as u64 + 1),
        ),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        plan_attempt_transition(
            &running(200),
            AttemptFence {
                version: 1,
                run_generation: 1,
            },
            AttemptAction::Complete {
                state: CheckState::Running,
            },
            Some(100),
        ),
        Err(Error::Invalid(_))
    ));
}

#[test]
fn terminal_states_reject_every_late_action() {
    for state in [
        CheckState::Success,
        CheckState::Fail,
        CheckState::Incomplete,
        CheckState::Cancelled,
    ] {
        let terminal = AttemptView {
            state,
            version: 2,
            run_generation: 2,
            lease_expires_at_ms: None,
        };
        for action in [
            AttemptAction::ClaimRun {
                lease_expires_at_ms: 300,
            },
            AttemptAction::Complete {
                state: CheckState::Success,
            },
            AttemptAction::Cancel,
            AttemptAction::RecoverIndeterminate,
            AttemptAction::RecoverExpired,
        ] {
            assert!(matches!(
                plan_attempt_transition(&terminal, terminal.fence(), action, Some(100)).unwrap(),
                AttemptTransitionOutcome::Refused(AttemptTransitionRefusal::InvalidState { .. })
            ));
        }
    }
}
