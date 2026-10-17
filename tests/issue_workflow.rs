use agentscm::Error;
use agentscm::context::ChangeId;
use agentscm::forge::{Forge, Host};
use agentscm::issue_workflow::*;

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const NATIVE: &str = "cccccccccccccccccccccccccccccccc";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn repo(path: &str) -> RepositoryIdentity {
    RepositoryIdentity {
        forge: Forge::GitHub,
        host: Host::GitHubCom,
        remote_id: 7,
        path: path.into(),
    }
}

fn issue(id: u64, number: u64, state: IssueState, basis: MatchBasis) -> IssueCandidate {
    IssueCandidate {
        identity: IssueIdentity {
            repository: repo("owner/project"),
            remote_id: id,
            number,
        },
        state,
        basis,
        request_digest: if basis == MatchBasis::ExactRequest {
            A
        } else {
            B
        }
        .into(),
        related_proposal: None,
    }
}

fn coverage<T>(candidates: Vec<T>) -> Coverage<T> {
    Coverage::Complete { candidates }
}

fn observation(open: Vec<IssueCandidate>) -> IntakeObservation {
    IntakeObservation {
        version: 1,
        repository: repo("owner/project"),
        open_issues: coverage(open),
        closed_issues: coverage(vec![]),
        proposals: coverage(vec![]),
        native_work: coverage(vec![]),
    }
}

fn request() -> IntakeRequest {
    IntakeRequest {
        version: 1,
        repository: repo("owner/project"),
        accepted_request_sha256: A.into(),
        native_repository: NATIVE.into(),
    }
}

#[test]
fn stable_identity_reuses_exact_issue_across_display_changes() {
    let mut candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
    candidate.identity.repository.path = "renamed/project".into();
    candidate.identity.number = 1040;
    let outcome = evaluate(&request(), observation(vec![candidate]), None).unwrap();
    let IntakeDecision::ReuseIssue { identity } = outcome.decision else {
        panic!("expected exact reuse")
    };
    assert_eq!(identity.stable_key(), "github:github.com:7:issue:41");
}

#[test]
fn complete_empty_is_eligible_but_each_incomplete_state_is_retained() {
    assert_eq!(
        evaluate(&request(), observation(vec![]), None)
            .unwrap()
            .decision,
        IntakeDecision::CreateEligible {}
    );
    for reason in [
        IncompleteReason::MorePages,
        IncompleteReason::Cancelled,
        IncompleteReason::AccessUnavailable,
    ] {
        let mut observed = observation(vec![]);
        observed.open_issues = Coverage::Incomplete {
            reason,
            candidates: vec![],
        };
        let outcome = evaluate(&request(), observed, None).unwrap();
        assert_eq!(outcome.decision, IntakeDecision::SearchIncomplete {});
        assert!(!outcome.trace.coverage_complete);
        assert!(
            matches!(outcome.coverage.open_issues, Coverage::Incomplete { reason: retained, .. } if retained == reason)
        );
    }
}

#[test]
fn similarity_is_never_an_automatic_match_and_can_be_separated() {
    let observed = observation(vec![issue(9, 9, IssueState::Open, MatchBasis::Similarity)]);
    assert!(matches!(
        evaluate(&request(), observed.clone(), None).unwrap().decision,
        IntakeDecision::ChoiceRequired { candidates } if candidates.len() == 1
    ));
    assert_eq!(
        evaluate(
            &request(),
            observed,
            Some(&OperatorChoice::SeparateIntent {})
        )
        .unwrap()
        .decision,
        IntakeDecision::CreateEligible {}
    );
}

#[test]
fn explicit_selection_requires_an_observed_open_issue() {
    let identity = issue(9, 9, IssueState::Open, MatchBasis::Similarity).identity;
    let choice = OperatorChoice::Select {
        candidate: CandidateReference::Issue {
            identity: identity.clone(),
            state: IssueState::Open,
        },
    };
    assert!(matches!(
        evaluate(&request(), observation(vec![]), Some(&choice)),
        Err(Error::Conflict(_))
    ));
    let outcome = evaluate(
        &request(),
        observation(vec![issue(9, 9, IssueState::Open, MatchBasis::Similarity)]),
        Some(&choice),
    )
    .unwrap();
    assert_eq!(outcome.decision, IntakeDecision::ReuseIssue { identity });
}

#[test]
fn ambiguous_exact_candidates_require_a_choice_in_stable_order() {
    let selected = issue(20, 20, IssueState::Open, MatchBasis::ExactRequest).identity;
    let outcome = evaluate(
        &request(),
        observation(vec![
            issue(20, 20, IssueState::Open, MatchBasis::ExactRequest),
            issue(10, 10, IssueState::Open, MatchBasis::ExactRequest),
        ]),
        None,
    )
    .unwrap();
    let IntakeDecision::ChoiceRequired { candidates } = outcome.decision else {
        panic!("expected ambiguity")
    };
    let keys = candidates
        .iter()
        .map(|candidate| match candidate {
            CandidateReference::Issue { identity, .. } => identity.stable_key(),
            _ => panic!("unexpected candidate"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "github:github.com:7:issue:10",
            "github:github.com:7:issue:20"
        ]
    );
    assert_eq!(
        evaluate(
            &request(),
            observation(vec![
                issue(10, 10, IssueState::Open, MatchBasis::ExactRequest),
                issue(20, 20, IssueState::Open, MatchBasis::ExactRequest),
            ]),
            Some(&OperatorChoice::Select {
                candidate: CandidateReference::Issue {
                    identity: selected.clone(),
                    state: IssueState::Open
                }
            }),
        )
        .unwrap()
        .decision,
        IntakeDecision::ReuseIssue { identity: selected }
    );
}

#[test]
fn closed_issue_and_related_proposal_are_terminal_descriptions() {
    let mut observed = observation(vec![]);
    observed.closed_issues = coverage(vec![issue(
        40,
        40,
        IssueState::Closed,
        MatchBasis::ExactRequest,
    )]);
    assert!(matches!(
        evaluate(&request(), observed, None).unwrap().decision,
        IntakeDecision::ClosedIssue { .. }
    ));

    let mut related = issue(41, 41, IssueState::Open, MatchBasis::ExactRequest);
    related.related_proposal = Some(ProposalIdentity {
        repository: repo("owner/project"),
        remote_id: 88,
        number: 12,
    });
    assert!(matches!(
        evaluate(&request(), observation(vec![related]), None)
            .unwrap()
            .decision,
        IntakeDecision::ExistingProposal { .. }
    ));
}

#[test]
fn selected_issue_with_native_work_surfaces_existing_change_after_intent_revision() {
    let candidate = issue(9, 9, IssueState::Open, MatchBasis::Similarity);
    let identity = candidate.identity.clone();
    let mut observed = observation(vec![candidate]);
    observed.native_work = coverage(vec![NativeWorkCandidate {
        issue: identity.clone(),
        change: ChangeId {
            repository: NATIVE.into(),
            id: "change-9".into(),
        },
        request_digest: B.into(),
    }]);
    let outcome = evaluate(
        &request(),
        observed,
        Some(&OperatorChoice::Select {
            candidate: CandidateReference::NativeWork {
                issue: identity,
                change: ChangeId {
                    repository: NATIVE.into(),
                    id: "change-9".into(),
                },
            },
        }),
    )
    .unwrap();
    assert!(matches!(
        outcome.decision,
        IntakeDecision::ExistingWork { .. }
    ));
}

#[test]
fn repository_mismatch_and_contradictory_duplicate_are_rejected() {
    let mut other = request();
    other.repository.remote_id = 8;
    assert!(matches!(
        evaluate(&other, observation(vec![]), None),
        Err(Error::Conflict(_))
    ));

    let first = issue(9, 9, IssueState::Open, MatchBasis::Similarity);
    let mut second = first.clone();
    second.request_digest = A.into();
    assert!(matches!(
        evaluate(&request(), observation(vec![first, second]), None),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn strict_decode_limits_and_request_digest_are_deterministic() {
    let mut value = serde_json::to_value(request()).unwrap();
    let valid = serde_json::to_vec(&value).unwrap();
    assert_eq!(IntakeRequest::from_slice(&valid).unwrap(), request());
    value
        .as_object_mut()
        .unwrap()
        .insert("extra".into(), serde_json::json!(true));
    assert!(IntakeRequest::from_slice(&serde_json::to_vec(&value).unwrap()).is_err());
    assert!(matches!(
        IntakeRequest::from_slice(&vec![b' '; MAX_INTAKE_BYTES + 1]),
        Err(Error::Limit(_))
    ));
    assert_eq!(
        accepted_request_digest(&repo("owner/project"), b"accepted intent").unwrap(),
        accepted_request_digest(&repo("owner/project"), b"accepted intent").unwrap()
    );
    assert_ne!(
        accepted_request_digest(&repo("owner/project"), b"accepted intent").unwrap(),
        accepted_request_digest(&repo("owner/project"), b"different intent").unwrap()
    );
}

#[test]
fn trace_and_decision_order_are_independent_of_candidate_order() {
    let a = issue(30, 30, IssueState::Open, MatchBasis::Similarity);
    let b = issue(10, 10, IssueState::Open, MatchBasis::Similarity);
    let first = evaluate(&request(), observation(vec![a.clone(), b.clone()]), None).unwrap();
    let second = evaluate(&request(), observation(vec![b, a]), None).unwrap();
    assert_eq!(first.decision, second.decision);
    assert_eq!(first.trace, second.trace);
}

fn proposal(id: u64, basis: MatchBasis) -> ProposalCandidate {
    ProposalCandidate {
        identity: ProposalIdentity {
            repository: repo("owner/project"),
            remote_id: id,
            number: id,
        },
        basis,
        request_digest: if basis == MatchBasis::ExactRequest {
            A
        } else {
            B
        }
        .into(),
    }
}

#[test]
fn unrelated_link_does_not_hide_exact_proposal() {
    for basis in [MatchBasis::ExactRequest, MatchBasis::Similarity] {
        let mut linked = issue(9, 9, IssueState::Open, basis);
        if basis == MatchBasis::ExactRequest {
            linked.request_digest = B.into();
        }
        linked.related_proposal = Some(proposal(88, MatchBasis::ExactRequest).identity.clone());
        let mut observed = observation(vec![linked]);
        observed.proposals = coverage(vec![proposal(88, MatchBasis::ExactRequest)]);
        assert!(matches!(
            evaluate(
                &request(),
                observed,
                Some(&OperatorChoice::SeparateIntent {})
            ),
            Err(Error::Conflict(_))
        ));
        let mut observed = observation(vec![]);
        observed.open_issues = coverage(vec![{
            let mut linked = issue(9, 9, IssueState::Open, basis);
            if basis == MatchBasis::ExactRequest {
                linked.request_digest = B.into();
            }
            linked.related_proposal = Some(proposal(88, MatchBasis::ExactRequest).identity);
            linked
        }]);
        observed.proposals = coverage(vec![proposal(88, MatchBasis::ExactRequest)]);
        assert!(matches!(
            evaluate(&request(), observed, None).unwrap().decision,
            IntakeDecision::ExistingProposal { .. }
        ));
    }
}

#[test]
fn incomplete_coverage_blocks_all_decisions_and_choices() {
    for class in 0..4 {
        for reason in [
            IncompleteReason::MorePages,
            IncompleteReason::Cancelled,
            IncompleteReason::AccessUnavailable,
        ] {
            let exact = issue(41, 41, IssueState::Open, MatchBasis::ExactRequest);
            let mut observed = observation(vec![exact.clone()]);
            match class {
                0 => {
                    observed.open_issues = Coverage::Incomplete {
                        reason,
                        candidates: vec![exact],
                    }
                }
                1 => {
                    observed.closed_issues = Coverage::Incomplete {
                        reason,
                        candidates: vec![],
                    }
                }
                2 => {
                    observed.proposals = Coverage::Incomplete {
                        reason,
                        candidates: vec![],
                    }
                }
                _ => {
                    observed.native_work = Coverage::Incomplete {
                        reason,
                        candidates: vec![],
                    }
                }
            }
            let result = evaluate(&request(), observed.clone(), None).unwrap();
            assert_eq!(result.decision, IntakeDecision::SearchIncomplete {});
            assert!(!result.trace.coverage_complete);
            assert!(!result.trace.candidate_keys.is_empty());
            assert!(matches!(
                evaluate(
                    &request(),
                    observed.clone(),
                    Some(&OperatorChoice::SeparateIntent {})
                ),
                Err(Error::Conflict(_))
            ));
            assert!(matches!(
                evaluate(
                    &request(),
                    observed,
                    Some(&OperatorChoice::Select {
                        candidate: CandidateReference::Issue {
                            identity: issue(41, 41, IssueState::Open, MatchBasis::ExactRequest)
                                .identity,
                            state: IssueState::Open
                        }
                    })
                ),
                Err(Error::Conflict(_))
            ));
        }
    }
}

#[test]
fn exact_terminal_references_are_canonical() {
    let p = proposal(88, MatchBasis::ExactRequest);
    let mut first = issue(1, 1, IssueState::Open, MatchBasis::ExactRequest);
    let mut second = issue(2, 2, IssueState::Open, MatchBasis::ExactRequest);
    first.related_proposal = Some(p.identity.clone());
    second.related_proposal = Some(p.identity.clone());
    let mut observed = observation(vec![first, second]);
    observed.proposals = coverage(vec![p.clone()]);
    assert_eq!(
        evaluate(&request(), observed, None).unwrap().decision,
        IntakeDecision::ExistingProposal {
            identity: p.identity
        }
    );
}

#[test]
fn repeated_similarity_proposals_have_one_stable_choice() {
    let p = proposal(88, MatchBasis::Similarity);
    let mut first = observation(vec![]);
    first.proposals = coverage(vec![p.clone(), p.clone()]);
    let mut second = observation(vec![]);
    second.proposals = coverage(vec![p]);
    let a = evaluate(&request(), first, None).unwrap();
    let b = evaluate(&request(), second, None).unwrap();
    assert_eq!(a.decision, b.decision);
    assert_eq!(a.trace, b.trace);
    assert!(
        matches!(a.decision, IntakeDecision::ChoiceRequired { candidates } if candidates.len() == 1)
    );
}

#[test]
fn selection_must_be_current_offered_canonical_result() {
    let a = issue(1, 1, IssueState::Open, MatchBasis::ExactRequest);
    let b = issue(2, 2, IssueState::Open, MatchBasis::Similarity);
    let selected_b = OperatorChoice::Select {
        candidate: CandidateReference::Issue {
            identity: b.identity.clone(),
            state: b.state,
        },
    };
    assert!(matches!(
        evaluate(
            &request(),
            observation(vec![a, b.clone()]),
            Some(&selected_b)
        ),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        evaluate(
            &request(),
            observation(vec![b]),
            Some(&OperatorChoice::Select {
                candidate: CandidateReference::Proposal {
                    identity: proposal(88, MatchBasis::Similarity).identity
                }
            })
        ),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn selection_resolves_each_ambiguous_terminal_kind() {
    for kind in 0..3 {
        let mut observed = observation(vec![]);
        match kind {
            0 => {
                observed.closed_issues = coverage(vec![
                    issue(1, 1, IssueState::Closed, MatchBasis::ExactRequest),
                    issue(2, 2, IssueState::Closed, MatchBasis::ExactRequest),
                ])
            }
            1 => {
                observed.proposals = coverage(vec![
                    proposal(1, MatchBasis::ExactRequest),
                    proposal(2, MatchBasis::ExactRequest),
                ])
            }
            _ => {
                observed.native_work = coverage(
                    vec![1, 2]
                        .into_iter()
                        .map(|id| NativeWorkCandidate {
                            issue: issue(id, id, IssueState::Open, MatchBasis::ExactRequest)
                                .identity,
                            change: ChangeId {
                                repository: NATIVE.into(),
                                id: format!("change-{id}"),
                            },
                            request_digest: A.into(),
                        })
                        .collect(),
                )
            }
        }
        let IntakeDecision::ChoiceRequired { candidates } =
            evaluate(&request(), observed.clone(), None)
                .unwrap()
                .decision
        else {
            panic!("expected choice")
        };
        assert_eq!(candidates.len(), 2);
        let decision = evaluate(
            &request(),
            observed,
            Some(&OperatorChoice::Select {
                candidate: candidates[0].clone(),
            }),
        )
        .unwrap()
        .decision;
        assert!(matches!(
            (kind, decision),
            (0, IntakeDecision::ClosedIssue { .. })
                | (1, IntakeDecision::ExistingProposal { .. })
                | (2, IntakeDecision::ExistingWork { .. })
        ));
    }
}

#[test]
fn cross_native_repository_is_rejected_before_decision() {
    let mut observed = observation(vec![]);
    observed.native_work = coverage(vec![NativeWorkCandidate {
        issue: issue(1, 1, IssueState::Open, MatchBasis::ExactRequest).identity,
        change: ChangeId {
            repository: "dddddddddddddddddddddddddddddddd".into(),
            id: "change-1".into(),
        },
        request_digest: A.into(),
    }]);
    assert!(matches!(
        evaluate(&request(), observed, None),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn matching_issue_proposal_and_work_is_one_native_choice() {
    let proposal = ProposalIdentity {
        repository: repo("owner/project"),
        remote_id: 90,
        number: 9,
    };
    let change = ChangeId {
        repository: NATIVE.into(),
        id: "existing".into(),
    };
    let mut matching = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
    matching.related_proposal = Some(proposal.clone());
    let mut found = observation(vec![matching.clone()]);
    found.proposals = coverage(vec![ProposalCandidate {
        identity: proposal,
        basis: MatchBasis::ExactRequest,
        request_digest: A.into(),
    }]);
    found.native_work = coverage(vec![NativeWorkCandidate {
        issue: matching.identity,
        change: change.clone(),
        request_digest: A.into(),
    }]);
    assert_eq!(
        evaluate(&request(), found, None).unwrap().decision,
        IntakeDecision::ExistingWork { change }
    );
}

#[test]
fn nested_identity_conflicts_are_rejected() {
    let proposal = ProposalIdentity {
        repository: repo("owner/project"),
        remote_id: 90,
        number: 9,
    };
    let mut linked = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
    linked.related_proposal = Some(proposal.clone());
    let mut found = observation(vec![linked.clone()]);
    let mut inconsistent = proposal;
    inconsistent.number = 99;
    found.proposals = coverage(vec![ProposalCandidate {
        identity: inconsistent,
        basis: MatchBasis::ExactRequest,
        request_digest: A.into(),
    }]);
    assert!(matches!(
        evaluate(&request(), found, None),
        Err(Error::Conflict(_))
    ));
    let mut found = observation(vec![linked.clone()]);
    let mut inconsistent_issue = linked.identity;
    inconsistent_issue.number = 99;
    found.native_work = coverage(vec![NativeWorkCandidate {
        issue: inconsistent_issue,
        change: ChangeId {
            repository: NATIVE.into(),
            id: "existing".into(),
        },
        request_digest: A.into(),
    }]);
    assert!(matches!(
        evaluate(&request(), found, None),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn observation_decoder_checks_nested_fields_and_aggregate_limit() {
    let found = observation(vec![issue(
        41,
        40,
        IssueState::Open,
        MatchBasis::ExactRequest,
    )]);
    let bytes = serde_json::to_vec(&found).unwrap();
    assert_eq!(IntakeObservation::from_slice(&bytes).unwrap(), found);
    let mut value = serde_json::to_value(&found).unwrap();
    value["open_issues"]["candidates"][0]["unknown"] = serde_json::json!(true);
    assert!(IntakeObservation::from_slice(&serde_json::to_vec(&value).unwrap()).is_err());
    let mut hundred = observation(
        (1..=100)
            .map(|n| issue(n, n, IssueState::Open, MatchBasis::ExactRequest))
            .collect(),
    );
    assert!(IntakeObservation::from_slice(&serde_json::to_vec(&hundred).unwrap()).is_ok());
    hundred.open_issues = coverage(
        (1..=101)
            .map(|n| issue(n, n, IssueState::Open, MatchBasis::ExactRequest))
            .collect(),
    );
    assert!(matches!(
        IntakeObservation::from_slice(&serde_json::to_vec(&hundred).unwrap()),
        Err(Error::Limit(_))
    ));
    assert!(matches!(
        IntakeObservation::from_slice(&vec![b' '; MAX_INTAKE_BYTES + 1]),
        Err(Error::Limit(_))
    ));
}

fn journal_fixture() -> (tempfile::TempDir, String) {
    let directory = tempfile::tempdir().unwrap();
    let authority = agentscm::authority::Authority::open(directory.path()).unwrap();
    let native = authority.repository_id().unwrap();
    drop(authority);
    (directory, native)
}

fn journal_request(native: &str) -> IntakeRequest {
    let mut value = request();
    value.native_repository = native.into();
    value
}

#[test]
fn typed_input_bounds_reject_without_changing_journal_state() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
    let registered = journal
        .register(
            "r",
            journal_request(&native),
            observation(vec![candidate.clone()]),
            None,
        )
        .unwrap();
    let acquired = journal
        .acquire(
            "a",
            &registered.outcome.key,
            "worker",
            None,
            &selection(1, 41, None),
        )
        .unwrap();
    let before = journal.intake(&registered.outcome.key).unwrap().unwrap();
    let registration = journal.operation("r").unwrap().unwrap();
    let acquisition = journal.operation("a").unwrap().unwrap();
    let counts = |journal: &IssueJournal| {
        let connection = rusqlite::Connection::open(journal.database_path()).unwrap();
        connection.query_row(
            "SELECT (SELECT count(*) FROM intakes), (SELECT count(*) FROM claims), (SELECT count(*) FROM operations)",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)),
        ).unwrap()
    };
    let prior_counts = counts(&journal);
    // This is a direct typed input; no transport decoder supplies the count bound.
    for reassess in [false, true] {
        let over_count = observation(vec![candidate.clone(); MAX_CANDIDATES + 1]);
        let result = if reassess {
            journal.reassess(
                "over-count-reassess",
                &before.key,
                before.generation,
                journal_request(&native),
                over_count,
                None,
            )
        } else {
            journal.register(
                "over-count-register",
                journal_request(&native),
                over_count,
                None,
            )
        };
        assert!(matches!(result, Err(Error::Limit(_))));
    }
    let similar = observation(vec![issue(
        41,
        40,
        IssueState::Open,
        MatchBasis::Similarity,
    )]);
    let mut invalid_issue = candidate.identity.clone();
    invalid_issue.remote_id = 0;
    let choices = [
        CandidateReference::NativeWork {
            issue: candidate.identity.clone(),
            change: ChangeId {
                repository: "x".repeat(MAX_INTAKE_BYTES + 1),
                id: "known".into(),
            },
        },
        CandidateReference::NativeWork {
            issue: candidate.identity.clone(),
            change: ChangeId {
                repository: native.clone(),
                id: "x".repeat(MAX_INTAKE_BYTES + 1),
            },
        },
        CandidateReference::NativeWork {
            issue: candidate.identity.clone(),
            change: ChangeId {
                repository: native.clone(),
                id: String::new(),
            },
        },
        CandidateReference::NativeWork {
            issue: candidate.identity.clone(),
            change: ChangeId {
                repository: native.clone(),
                id: "invalid choice".into(),
            },
        },
        CandidateReference::NativeWork {
            issue: invalid_issue,
            change: ChangeId {
                repository: native.clone(),
                id: "known".into(),
            },
        },
    ];
    for candidate in choices {
        let choice = OperatorChoice::Select { candidate };
        assert!(matches!(
            evaluate(&journal_request(&native), similar.clone(), Some(&choice)),
            Err(Error::Invalid(_) | Error::Limit(_))
        ));
        assert!(matches!(
            journal.register(
                "bad-choice-register",
                journal_request(&native),
                similar.clone(),
                Some(choice.clone())
            ),
            Err(Error::Invalid(_) | Error::Limit(_))
        ));
        assert!(matches!(
            journal.reassess(
                "bad-choice-reassess",
                &before.key,
                before.generation,
                journal_request(&native),
                similar.clone(),
                Some(choice)
            ),
            Err(Error::Invalid(_) | Error::Limit(_))
        ));
    }
    assert_eq!(journal.intake(&before.key).unwrap().unwrap(), before);
    assert_eq!(
        journal.claim(&candidate.identity).unwrap().unwrap(),
        acquired.outcome
    );
    assert_eq!(journal.operation("r").unwrap().unwrap(), registration);
    assert_eq!(journal.operation("a").unwrap().unwrap(), acquisition);
    for operation in [
        "over-count-register",
        "over-count-reassess",
        "bad-choice-register",
        "bad-choice-reassess",
    ] {
        assert!(journal.operation(operation).unwrap().is_none());
    }
    assert_eq!(counts(&journal), prior_counts);
}

#[test]
fn journal_reuses_equivalent_intake_and_reservation_never_rearms() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let first = journal
        .register("r1", journal_request(&native), observation(vec![]), None)
        .unwrap();
    let second = journal
        .register("r2", journal_request(&native), observation(vec![]), None)
        .unwrap();
    assert_eq!(first.outcome.key, second.outcome.key);
    let key = first.outcome.key;
    let reserved = journal.reserve_create("create-1", &key, 1).unwrap();
    assert_eq!(
        reserved.outcome.create_state,
        CreateState::ReservedUncertain
    );
    drop(journal);
    let mut reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    assert!(reopened.reserve_create("create-2", &key, 2).is_err());
    assert!(
        reopened
            .reassess(
                "retry-search",
                &key,
                2,
                journal_request(&native),
                observation(vec![]),
                None
            )
            .is_err()
    );
    let replay = reopened.reserve_create("create-1", &key, 1).unwrap();
    assert!(replay.replay);
    assert_eq!(replay.outcome.create_state, CreateState::ReservedUncertain);
}

#[test]
fn journal_claim_fences_progress_and_replay() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let first = journal
        .register(
            "r1",
            journal_request(&native),
            observation(vec![issue(
                41,
                40,
                IssueState::Open,
                MatchBasis::ExactRequest,
            )]),
            None,
        )
        .unwrap();
    let key = first.outcome.key;
    let selected = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest).identity;
    let owned = journal
        .acquire("a1", &key, "worker-1", None, &selection(1, 41, None))
        .unwrap();
    assert_eq!(owned.outcome.generation, 1);
    let progress = journal
        .mutate_claim(
            "p1",
            &selected,
            "worker-1",
            1,
            ClaimAction::Progress {
                digest: B.into(),
                change: None,
            },
        )
        .unwrap();
    assert_eq!(progress.outcome.generation, 2);
    assert!(
        journal
            .mutate_claim("stale", &selected, "worker-1", 1, ClaimAction::Release)
            .is_err()
    );
    let replay = journal
        .acquire("a1", &key, "worker-1", None, &selection(1, 41, None))
        .unwrap();
    assert!(replay.replay);
    assert!(!replay.current_fence);
    let released = journal
        .mutate_claim("release", &selected, "worker-1", 2, ClaimAction::Release)
        .unwrap();
    assert!(!released.current_fence);
    assert_eq!(released.outcome.progress_digest.as_deref(), Some(B));
    let next = journal
        .acquire("a2", &key, "worker-2", Some(3), &selection(2, 41, None))
        .unwrap();
    assert_eq!(next.outcome.progress_digest.as_deref(), Some(B));
    assert_eq!(next.outcome.generation, 4);
}

#[test]
fn journal_refuses_scope_mismatch_and_exclusive_recovery_lease() {
    let (directory, native) = journal_fixture();
    let journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    assert!(IssueJournal::open(directory.path(), &native, "installation-2").is_err());
    assert!(IssueJournal::open(directory.path(), NATIVE, "installation-1").is_err());
    assert!(agentscm::local_recovery::AccessGuard::exclusive(directory.path()).is_err());
    drop(journal);
    assert!(agentscm::local_recovery::AccessGuard::exclusive(directory.path()).is_ok());
}

#[test]
fn journal_reconciliation_is_exact_and_conflicts_on_other_issue() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let registered = journal
        .register("r1", journal_request(&native), observation(vec![]), None)
        .unwrap();
    let key = registered.outcome.key;
    journal.reserve_create("create-1", &key, 1).unwrap();
    let observed = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
    let reconciled = journal
        .reconcile_create(
            "create-1",
            &key,
            observed.identity.clone(),
            observed.clone(),
        )
        .unwrap();
    assert_eq!(reconciled.create_state, CreateState::Observed);
    assert_eq!(
        journal
            .reconcile_create("create-1", &key, observed.identity.clone(), observed)
            .unwrap()
            .generation,
        reconciled.generation
    );
    let other = issue(42, 41, IssueState::Open, MatchBasis::ExactRequest);
    assert!(
        journal
            .reconcile_create("create-1", &key, other.identity.clone(), other)
            .is_err()
    );
    assert!(
        journal
            .reserve_create("create-2", &key, reconciled.generation)
            .is_err()
    );
}

#[test]
fn operation_ids_bind_full_input() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    journal
        .register(
            "same-id",
            journal_request(&native),
            observation(vec![]),
            None,
        )
        .unwrap();
    let changed = observation(vec![issue(
        41,
        40,
        IssueState::Open,
        MatchBasis::ExactRequest,
    )]);
    assert!(matches!(
        journal.register("same-id", journal_request(&native), changed, None),
        Err(Error::Conflict(_))
    ));
}

#[test]
fn concurrent_connections_allow_one_canonical_claim() {
    use std::sync::{Arc, Barrier};
    let (directory, native) = journal_fixture();
    drop(IssueJournal::open(directory.path(), &native, "installation-1").unwrap());
    let root = directory.path().to_path_buf();
    let start = Arc::new(Barrier::new(2));
    let mut threads = Vec::new();
    for n in 0..2 {
        let root = root.clone();
        let native = native.clone();
        let start = start.clone();
        threads.push(std::thread::spawn(move || {
            start.wait();
            let mut journal = IssueJournal::open(&root, &native, "installation-1").unwrap();
            let receipt = journal
                .register(
                    &format!("r{n}"),
                    journal_request(&native),
                    observation(vec![issue(
                        41,
                        40,
                        IssueState::Open,
                        MatchBasis::ExactRequest,
                    )]),
                    None,
                )
                .unwrap();
            journal
                .acquire(
                    &format!("a{n}"),
                    &receipt.outcome.key,
                    &format!("worker-{n}"),
                    None,
                    &selection(receipt.outcome.generation, 41, None),
                )
                .is_ok()
        }));
    }
    let wins = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .filter(|won| *won)
        .count();
    assert_eq!(wins, 1);
}

#[test]
fn aliases_for_one_issue_share_claim_and_stale_worker_cannot_release() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let first = journal
        .register(
            "r1",
            journal_request(&native),
            observation(vec![issue(
                41,
                40,
                IssueState::Open,
                MatchBasis::ExactRequest,
            )]),
            None,
        )
        .unwrap();
    let mut other_request = journal_request(&native);
    other_request.accepted_request_sha256 = B.into();
    let mut similar = issue(41, 40, IssueState::Open, MatchBasis::Similarity);
    similar.request_digest = A.into();
    let second = journal
        .register(
            "r2",
            other_request,
            observation(vec![similar.clone()]),
            Some(OperatorChoice::Select {
                candidate: CandidateReference::Issue {
                    identity: similar.identity.clone(),
                    state: IssueState::Open,
                },
            }),
        )
        .unwrap();
    assert_ne!(first.outcome.key, second.outcome.key);
    let claim = journal
        .acquire(
            "a1",
            &first.outcome.key,
            "worker-1",
            None,
            &selection(1, 41, None),
        )
        .unwrap();
    assert!(
        journal
            .acquire(
                "a2",
                &second.outcome.key,
                "worker-2",
                Some(claim.outcome.generation),
                &selection(1, 41, None)
            )
            .is_err()
    );
    let transferred = journal
        .mutate_claim(
            "t1",
            &similar.identity,
            "worker-1",
            1,
            ClaimAction::Transfer {
                worker_id: "worker-2".into(),
            },
        )
        .unwrap();
    assert_eq!(transferred.outcome.generation, 2);
    assert!(
        journal
            .mutate_claim(
                "old",
                &similar.identity,
                "worker-1",
                1,
                ClaimAction::Release
            )
            .is_err()
    );
    assert!(
        journal
            .mutate_claim(
                "old2",
                &similar.identity,
                "worker-1",
                2,
                ClaimAction::Release
            )
            .is_err()
    );
}

#[test]
fn reconciliation_updates_existing_rows_at_aggregate_cap() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let registered = journal
        .register("r1", journal_request(&native), observation(vec![]), None)
        .unwrap();
    let key = registered.outcome.key;
    journal.reserve_create("create-1", &key, 1).unwrap();
    fill_operation_capacity(&journal, "r1");
    assert!(
        journal
            .register("r1", journal_request(&native), observation(vec![]), None)
            .unwrap()
            .replay
    );
    assert!(journal.reserve_create("create-1", &key, 1).unwrap().replay);
    assert!(matches!(
        journal.register(
            "beyond-cap",
            journal_request(&native),
            observation(vec![]),
            None
        ),
        Err(Error::Limit(_))
    ));
    let observed = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
    let final_state = journal
        .reconcile_create("create-1", &key, observed.identity.clone(), observed)
        .unwrap();
    assert_eq!(final_state.create_state, CreateState::Observed);
}

#[test]
fn metadata_and_scalar_json_corruption_fail_closed() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let registered = journal
        .register("r1", journal_request(&native), observation(vec![]), None)
        .unwrap();
    let path = journal.database_path().to_path_buf();
    drop(journal);
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute(
            "UPDATE intakes SET generation=2 WHERE key=?1",
            [&registered.outcome.key],
        )
        .unwrap();
    drop(connection);
    assert!(IssueJournal::open(directory.path(), &native, "installation-1").is_err());
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection
        .execute("UPDATE intakes SET generation=1", [])
        .unwrap();
    connection
        .execute("UPDATE metadata SET installation_id='other'", [])
        .unwrap();
    drop(connection);
    assert!(IssueJournal::open(directory.path(), &native, "installation-1").is_err());
}

#[test]
fn journal_open_refuses_quarantined_root() {
    let (directory, native) = journal_fixture();
    let quarantine = directory.path().join(".agentscm-recovery");
    std::fs::create_dir_all(&quarantine).unwrap();
    std::fs::write(quarantine.join("quarantine.json"), b"{}").unwrap();
    assert!(IssueJournal::open(directory.path(), &native, "installation-1").is_err());
}

fn selection(generation: i64, id: u64, change: Option<ChangeId>) -> AcquireIntent {
    AcquireIntent {
        intake_generation: generation,
        issue: issue(id, id, IssueState::Open, MatchBasis::ExactRequest).identity,
        selected_change: change,
    }
}

// Fixture receipts remain terminal and internally consistent, including when
// their operation IDs are cloned. All seed writes share one transaction.
fn receipt_checksum(operation: &OperationRecord, bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let scope = serde_json::to_vec(&(
        &operation.id,
        &operation.kind,
        &operation.intake_key,
        &operation.issue_key,
        operation.expected_generation,
        &operation.request_digest,
        operation.expected_intake_generation,
        &operation.selected_change,
    ))
    .unwrap();
    let mut digest = Sha256::new();
    for part in [
        b"agentscm.issue-outcome.v1".as_slice(),
        scope.as_slice(),
        bytes,
    ] {
        digest.update((part.len() as u64).to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

fn fill_operation_capacity(journal: &IssueJournal, template: &str) {
    let connection = rusqlite::Connection::open(journal.database_path()).unwrap();
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT outcome_json FROM operations WHERE id=?1",
            [template],
            |r| r.get(0),
        )
        .unwrap();
    let mut operation = journal.operation(template).unwrap().unwrap();
    let seed = connection.unchecked_transaction().unwrap();
    let count:i64=seed.query_row("SELECT (SELECT count(*) FROM intakes)+(SELECT count(*) FROM claims)+(SELECT count(*) FROM operations)",[],|r|r.get(0)).unwrap();
    for n in count..4096 {
        operation.id = format!("seed-{n}");
        let checksum = receipt_checksum(&operation, &bytes);
        seed.execute("INSERT INTO operations SELECT ?1,kind,intake_key,issue_key,expected_generation,request_digest,expected_intake_generation,selected_change_repository,selected_change_id,?2,outcome_json FROM operations WHERE id=?3",rusqlite::params![operation.id,checksum,template]).unwrap();
    }
    seed.commit().unwrap();
}

#[test]
fn claimed_intake_is_sticky_and_replay_uses_original_selection() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let found = observation(vec![issue(
        1,
        1,
        IssueState::Open,
        MatchBasis::ExactRequest,
    )]);
    let record = journal
        .register("r", journal_request(&native), found.clone(), None)
        .unwrap()
        .outcome;
    let intent = selection(1, 1, None);
    let owned = journal
        .acquire("a", &record.key, "worker", None, &intent)
        .unwrap();
    let bound = journal.intake(&record.key).unwrap().unwrap();
    assert_eq!(bound.generation, 2);
    assert_eq!(
        bound.claimed_issue.as_ref().unwrap().stable_key(),
        intent.issue.stable_key()
    );
    let blocked = journal
        .reassess(
            "move",
            &record.key,
            2,
            journal_request(&native),
            observation(vec![]),
            None,
        )
        .unwrap();
    assert!(blocked.outcome.assessment_conflicted);
    assert!(!blocked.current_fence);
    let refreshed = journal
        .reassess(
            "refresh",
            &record.key,
            3,
            journal_request(&native),
            found.clone(),
            None,
        )
        .unwrap()
        .outcome;
    assert_eq!(refreshed.generation, 4);
    assert!(journal.reserve_create("create", &record.key, 4).is_err());
    assert!(
        journal
            .acquire("stale", &record.key, "worker", None, &intent)
            .is_err()
    );
    let replay = journal
        .acquire("a", &record.key, "worker", None, &intent)
        .unwrap();
    assert_eq!(replay.outcome, owned.outcome);
    assert!(replay.current_fence);
    journal
        .mutate_claim("release", &intent.issue, "worker", 1, ClaimAction::Release)
        .unwrap();
    assert!(
        !journal
            .acquire("a", &record.key, "worker", None, &intent)
            .unwrap()
            .current_fence
    );
    assert!(
        journal
            .acquire(
                "wrong-issue",
                &record.key,
                "next",
                Some(2),
                &selection(4, 2, None)
            )
            .is_err()
    );
    let new = journal
        .acquire("next", &record.key, "next", Some(2), &selection(4, 1, None))
        .unwrap();
    assert_eq!(new.outcome.generation, 3);
    assert!(
        journal
            .claim(&selection(1, 2, None).issue)
            .unwrap()
            .is_none()
    );
    // Later contradictory evidence blocks new actions but cannot alter replay.
    assert!(
        journal
            .register(
                "contradiction",
                journal_request(&native),
                observation(vec![]),
                None
            )
            .is_err()
    );
    assert!(
        journal
            .intake(&record.key)
            .unwrap()
            .unwrap()
            .assessment_conflicted
    );
    assert_eq!(
        journal
            .acquire("a", &record.key, "worker", None, &intent)
            .unwrap()
            .outcome,
        owned.outcome
    );
    assert!(
        !journal
            .acquire("a", &record.key, "worker", None, &intent)
            .unwrap()
            .current_fence
    );
}

fn native_observation(native: &str, id: &str) -> IntakeObservation {
    let mut observed = observation(vec![issue(
        41,
        40,
        IssueState::Open,
        MatchBasis::ExactRequest,
    )]);
    observed.native_work = coverage(vec![NativeWorkCandidate {
        issue: issue(41, 40, IssueState::Open, MatchBasis::ExactRequest).identity,
        change: ChangeId {
            repository: native.into(),
            id: id.into(),
        },
        request_digest: A.into(),
    }]);
    observed
}

#[test]
fn released_claim_merges_native_observation_and_preserves_progress() {
    for initial in [None, Some("known"), Some("different")] {
        let (directory, native) = journal_fixture();
        let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        let first = journal
            .register(
                "r",
                journal_request(&native),
                observation(vec![issue(
                    41,
                    40,
                    IssueState::Open,
                    MatchBasis::ExactRequest,
                )]),
                None,
            )
            .unwrap()
            .outcome;
        let selected = selection(1, 41, None);
        journal
            .acquire("a", &first.key, "first", None, &selected)
            .unwrap();
        let initial_change = initial.map(|id| ChangeId {
            repository: native.clone(),
            id: id.into(),
        });
        journal
            .mutate_claim(
                "p",
                &selected.issue,
                "first",
                1,
                ClaimAction::Progress {
                    digest: B.into(),
                    change: initial_change.clone(),
                },
            )
            .unwrap();
        journal
            .mutate_claim("release", &selected.issue, "first", 2, ClaimAction::Release)
            .unwrap();
        let refreshed = journal
            .reassess(
                "refresh",
                &first.key,
                2,
                journal_request(&native),
                native_observation(&native, "known"),
                None,
            )
            .unwrap()
            .outcome;
        let observed_change = ChangeId {
            repository: native.clone(),
            id: "known".into(),
        };
        assert_eq!(
            refreshed.assessment_conflicted,
            initial == Some("different")
        );
        let intent = selection(refreshed.generation, 41, Some(observed_change.clone()));
        assert!(
            journal
                .acquire(
                    "wrong-selection",
                    &first.key,
                    "second",
                    Some(3),
                    &selection(refreshed.generation, 41, None)
                )
                .is_err()
        );
        let acquired = journal.acquire("again", &first.key, "second", Some(3), &intent);
        if initial == Some("different") {
            assert!(matches!(acquired, Err(Error::Conflict(_))));
            let current = journal.claim(&selected.issue).unwrap().unwrap();
            assert_eq!(current.change, initial_change);
            assert_eq!(current.generation, 3);
            assert!(current.worker_id.is_none());
        } else {
            let acquired = acquired.unwrap();
            assert_eq!(acquired.outcome.change, Some(observed_change.clone()));
            assert_eq!(acquired.outcome.progress_digest.as_deref(), Some(B));
            let transferred = journal
                .mutate_claim(
                    "transfer",
                    &selected.issue,
                    "second",
                    4,
                    ClaimAction::Transfer {
                        worker_id: "third".into(),
                    },
                )
                .unwrap();
            assert_eq!(transferred.outcome.change, Some(observed_change.clone()));
            let released = journal
                .mutate_claim(
                    "release-again",
                    &selected.issue,
                    "third",
                    5,
                    ClaimAction::Release,
                )
                .unwrap();
            assert_eq!(released.outcome.change, Some(observed_change));
            assert_eq!(released.outcome.progress_digest.as_deref(), Some(B));
        }
    }
}

#[test]
fn contradictory_registration_invalidates_readiness_and_requires_reload() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let empty = journal
        .register("r", journal_request(&native), observation(vec![]), None)
        .unwrap()
        .outcome;
    let mut old_writer = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let exact = observation(vec![issue(
        41,
        40,
        IssueState::Open,
        MatchBasis::ExactRequest,
    )]);
    assert!(matches!(
        journal.register(
            "contradiction",
            journal_request(&native),
            exact.clone(),
            None
        ),
        Err(Error::Conflict(_))
    ));
    let blocked = old_writer.intake(&empty.key).unwrap().unwrap();
    assert_eq!(blocked.generation, 2);
    assert!(blocked.assessment_conflicted);
    assert!(
        old_writer
            .reserve_create("old-ready", &empty.key, 1)
            .is_err()
    );
    assert!(
        old_writer
            .reserve_create("new-ready", &empty.key, 2)
            .is_err()
    );
    assert!(
        journal
            .register(
                "contradiction",
                journal_request(&native),
                exact.clone(),
                None
            )
            .is_err()
    );
    assert_eq!(journal.intake(&empty.key).unwrap().unwrap(), blocked);
    assert!(
        journal
            .register(
                "contradiction",
                journal_request(&native),
                observation(vec![]),
                None
            )
            .is_err()
    );
    let recovered = journal
        .reassess(
            "refresh",
            &empty.key,
            2,
            journal_request(&native),
            exact.clone(),
            None,
        )
        .unwrap()
        .outcome;
    assert!(!recovered.assessment_conflicted);
    assert!(
        journal
            .register("contradiction", journal_request(&native), exact, None)
            .is_err()
    );
    assert_eq!(journal.intake(&empty.key).unwrap().unwrap(), recovered);
    assert!(
        journal
            .register("r", journal_request(&native), observation(vec![]), None)
            .unwrap()
            .replay
    );
    assert_eq!(journal.intake(&empty.key).unwrap().unwrap(), recovered);
}

#[test]
fn contradictory_registration_does_not_rearm_reserved_or_observed_state() {
    for (reconcile, kind) in [false, true]
        .into_iter()
        .flat_map(|reconcile| (0..3).map(move |kind| (reconcile, kind)))
    {
        let (directory, native) = journal_fixture();
        let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        let empty = journal
            .register("r", journal_request(&native), observation(vec![]), None)
            .unwrap()
            .outcome;
        journal.reserve_create("create", &empty.key, 1).unwrap();
        let candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
        if reconcile {
            journal
                .reconcile_create(
                    "create",
                    &empty.key,
                    candidate.identity.clone(),
                    candidate.clone(),
                )
                .unwrap();
        }
        let prior = journal.intake(&empty.key).unwrap().unwrap();
        let mut contradiction = observation(vec![candidate]);
        match kind {
            0 => {
                if let Coverage::Complete { candidates } = &mut contradiction.open_issues {
                    candidates[0].related_proposal = Some(ProposalIdentity {
                        repository: repo("owner/project"),
                        remote_id: 9,
                        number: 9,
                    });
                }
            }
            1 => {
                contradiction.open_issues = coverage(vec![]);
                contradiction.closed_issues = coverage(vec![issue(
                    41,
                    40,
                    IssueState::Closed,
                    MatchBasis::ExactRequest,
                )]);
            }
            _ => {
                contradiction.proposals = Coverage::Incomplete {
                    reason: IncompleteReason::MorePages,
                    candidates: vec![],
                }
            }
        }
        assert!(
            journal
                .register(
                    "contradiction",
                    journal_request(&native),
                    contradiction.clone(),
                    None
                )
                .is_err()
        );
        let blocked = journal.intake(&empty.key).unwrap().unwrap();
        assert!(
            journal
                .register("repeat", journal_request(&native), contradiction, None)
                .is_err()
        );
        assert_eq!(journal.intake(&empty.key).unwrap().unwrap(), blocked);
        assert!(blocked.assessment_conflicted);
        assert_eq!(blocked.generation, prior.generation + 1);
        assert_eq!(blocked.create_state, prior.create_state);
        assert_eq!(blocked.create_operation, prior.create_operation);
        assert_eq!(blocked.selected_issue, prior.selected_issue);
        assert_eq!(blocked.reconciled_observation, prior.reconciled_observation);
        assert!(
            journal
                .reserve_create("again", &empty.key, blocked.generation)
                .is_err()
        );
        let current = if reconcile {
            blocked
        } else {
            let candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
            journal
                .reconcile_create("create", &empty.key, candidate.identity.clone(), candidate)
                .unwrap()
        };
        assert!(current.assessment_conflicted);
        assert!(
            journal
                .acquire(
                    "old",
                    &empty.key,
                    "w",
                    None,
                    &selection(prior.generation, 41, None)
                )
                .is_err()
        );
        let mut reloaded = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        assert!(
            reloaded
                .acquire(
                    "reload",
                    &empty.key,
                    "w",
                    None,
                    &selection(current.generation, 41, None)
                )
                .is_err()
        );
    }
}

#[test]
fn conflicting_registration_invalidates_even_at_capacity() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let empty = journal
        .register("r", journal_request(&native), observation(vec![]), None)
        .unwrap()
        .outcome;
    fill_operation_capacity(&journal, "r");
    assert!(
        journal
            .register(
                "contradiction",
                journal_request(&native),
                observation(vec![issue(
                    41,
                    40,
                    IssueState::Open,
                    MatchBasis::ExactRequest
                )]),
                None
            )
            .is_err()
    );
    let blocked = journal.intake(&empty.key).unwrap().unwrap();
    assert_eq!(blocked.generation, 2);
    assert!(blocked.assessment_conflicted);
    assert!(journal.operation("contradiction").unwrap().is_none());
    assert!(journal.reserve_create("old-ready", &empty.key, 1).is_err());
    assert!(journal.reserve_create("new-ready", &empty.key, 2).is_err());
    assert!(
        journal
            .register(
                "contradiction",
                journal_request(&native),
                observation(vec![]),
                None
            )
            .is_err()
    );
    assert_eq!(journal.intake(&empty.key).unwrap().unwrap(), blocked);
    assert!(
        journal
            .register("r", journal_request(&native), observation(vec![]), None)
            .unwrap()
            .replay
    );
    drop(journal);
    assert!(
        IssueJournal::open(directory.path(), &native, "installation-1")
            .unwrap()
            .intake(&empty.key)
            .unwrap()
            .unwrap()
            .assessment_conflicted
    );
}

#[test]
fn equivalent_registration_canonicalizes_order_and_identical_duplicates() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let first = issue(1, 1, IssueState::Open, MatchBasis::Similarity);
    let second = issue(2, 2, IssueState::Open, MatchBasis::Similarity);
    let record = journal
        .register(
            "r1",
            journal_request(&native),
            observation(vec![first.clone(), second.clone()]),
            None,
        )
        .unwrap()
        .outcome;
    assert_eq!(
        journal
            .register(
                "r2",
                journal_request(&native),
                observation(vec![second, first.clone(), first]),
                None
            )
            .unwrap()
            .outcome,
        record
    );
}

#[test]
fn preexisting_empty_or_orphan_journal_never_initializes() {
    for empty_file in [false, true] {
        let (directory, native) = journal_fixture();
        let namespace = directory.path().join("issue-workflow");
        std::fs::create_dir(&namespace).unwrap();
        if empty_file {
            std::fs::write(namespace.join("state.sqlite"), b"").unwrap();
        }
        assert!(IssueJournal::open(directory.path(), &native, "installation-1").is_err());
        assert_eq!(namespace.join("state.sqlite").exists(), empty_file);
    }
}

#[test]
fn reservation_survives_database_loss_as_fail_closed_reopen() {
    for truncate in [false, true] {
        let (directory, native) = journal_fixture();
        let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        let empty = journal
            .register("r", journal_request(&native), observation(vec![]), None)
            .unwrap()
            .outcome;
        journal.reserve_create("create", &empty.key, 1).unwrap();
        let path = journal.database_path().to_owned();
        drop(journal);
        if truncate {
            std::fs::write(&path, b"").unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
        assert!(IssueJournal::open(directory.path(), &native, "installation-1").is_err());
    }
}

#[test]
fn concurrent_first_open_has_one_initializer_and_safe_retry() {
    use std::sync::{Arc, Barrier};
    let (directory, native) = journal_fixture();
    let root = directory.path().to_owned();
    let start = Arc::new(Barrier::new(2));
    let threads = (0..2)
        .map(|_| {
            let root = root.clone();
            let native = native.clone();
            let start = start.clone();
            std::thread::spawn(move || {
                start.wait();
                IssueJournal::open(&root, &native, "installation-1").is_ok()
            })
        })
        .collect::<Vec<_>>();
    assert!(
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|ok| *ok)
            .count()
            >= 1
    );
    assert!(IssueJournal::open(&root, &native, "installation-1").is_ok());
}

#[test]
fn oversized_journal_input_blocks_existing_readiness_without_initial_partial_rows() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let connection = rusqlite::Connection::open(journal.database_path()).unwrap();
    let count = || {
        connection.query_row("SELECT (SELECT count(*) FROM intakes)+(SELECT count(*) FROM claims)+(SELECT count(*) FROM operations)",[],|r|r.get::<_,i64>(0)).unwrap()
    };
    let large = observation(
        (1..=100)
            .map(|id| issue(id, id, IssueState::Open, MatchBasis::ExactRequest))
            .collect(),
    );
    assert!(serde_json::to_vec(&large).unwrap().len() > 16384);
    assert!(matches!(
        journal.register("too-large", journal_request(&native), large.clone(), None),
        Err(Error::Limit(_))
    ));
    assert_eq!(count(), 0);
    let empty = journal
        .register("r", journal_request(&native), observation(vec![]), None)
        .unwrap()
        .outcome;
    let prior = count();
    let blocked = journal
        .reassess(
            "too-large-reassess",
            &empty.key,
            1,
            journal_request(&native),
            large.clone(),
            None,
        )
        .unwrap()
        .outcome;
    assert!(blocked.assessment_conflicted);
    assert_eq!(blocked.generation, 2);
    assert_eq!(blocked.outcome, empty.outcome);
    assert_eq!(count(), prior + 1);
    assert!(journal.reserve_create("stale", &empty.key, 1).is_err());
    assert!(journal.reserve_create("reloaded", &empty.key, 2).is_err());
    assert!(
        journal
            .acquire(
                "blocked-acquire",
                &empty.key,
                "w",
                None,
                &selection(2, 41, None)
            )
            .is_err()
    );
    let recovered = journal
        .reassess(
            "recover",
            &empty.key,
            2,
            journal_request(&native),
            observation(vec![]),
            None,
        )
        .unwrap()
        .outcome;
    let replay = journal
        .reassess(
            "too-large-reassess",
            &empty.key,
            1,
            journal_request(&native),
            large.clone(),
            None,
        )
        .unwrap();
    assert!(replay.replay);
    assert_eq!(replay.outcome, blocked);
    assert_eq!(journal.intake(&empty.key).unwrap().unwrap(), recovered);
    assert!(
        journal
            .register("large-conflict", journal_request(&native), large, None)
            .is_err()
    );
    let invalidated = journal.intake(&empty.key).unwrap().unwrap();
    assert!(invalidated.assessment_conflicted);
    assert_eq!(invalidated.generation, 4);
    assert!(journal.operation("too-large").unwrap().is_none());
    drop(connection);
    drop(journal);
    let mut reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    assert_eq!(reopened.intake(&empty.key).unwrap().unwrap(), invalidated);
    assert!(
        reopened
            .reserve_create("reopen-create", &empty.key, 4)
            .is_err()
    );
    assert!(
        reopened
            .acquire(
                "reopen-acquire",
                &empty.key,
                "w",
                None,
                &selection(4, 41, None)
            )
            .is_err()
    );
}

#[test]
fn reconciled_existing_proposal_prevents_new_claim() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let empty = journal
        .register("r", journal_request(&native), observation(vec![]), None)
        .unwrap()
        .outcome;
    journal.reserve_create("create", &empty.key, 1).unwrap();
    let mut candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
    candidate.related_proposal = Some(proposal(90, MatchBasis::ExactRequest).identity);
    let reconciled = journal
        .reconcile_create("create", &empty.key, candidate.identity.clone(), candidate)
        .unwrap();
    assert!(
        journal
            .acquire(
                "a",
                &empty.key,
                "worker",
                None,
                &selection(reconciled.generation, 41, None)
            )
            .is_err()
    );
    assert!(
        journal
            .claim(&selection(1, 41, None).issue)
            .unwrap()
            .is_none()
    );
    assert_eq!(journal.intake(&empty.key).unwrap().unwrap(), reconciled);
    assert!(
        journal
            .reserve_create("again", &empty.key, reconciled.generation)
            .is_err()
    );
    assert!(journal.operation("a").unwrap().is_none());
    drop(journal);
    assert_eq!(
        IssueJournal::open(directory.path(), &native, "installation-1")
            .unwrap()
            .intake(&empty.key)
            .unwrap()
            .unwrap(),
        reconciled
    );
}

#[test]
fn shared_proposal_terminal_is_removed_after_all_matching_chains() {
    for workers in [1, 2] {
        let p = proposal(90, MatchBasis::ExactRequest).identity;
        let mut first = issue(1, 1, IssueState::Open, MatchBasis::ExactRequest);
        first.related_proposal = Some(p.clone());
        let mut second = issue(2, 2, IssueState::Open, MatchBasis::ExactRequest);
        second.related_proposal = Some(p.clone());
        let mut observed = observation(vec![first.clone(), second.clone()]);
        observed.proposals = coverage(vec![ProposalCandidate {
            identity: p,
            basis: MatchBasis::ExactRequest,
            request_digest: A.into(),
        }]);
        observed.native_work = coverage(if workers == 1 {
            vec![NativeWorkCandidate {
                issue: second.identity,
                change: ChangeId {
                    repository: NATIVE.into(),
                    id: "work-2".into(),
                },
                request_digest: A.into(),
            }]
        } else {
            [first.identity, second.identity]
                .into_iter()
                .map(|issue| NativeWorkCandidate {
                    change: ChangeId {
                        repository: NATIVE.into(),
                        id: format!("work-{}", issue.remote_id),
                    },
                    issue,
                    request_digest: A.into(),
                })
                .collect()
        });
        let result = evaluate(&request(), observed, None).unwrap();
        if workers == 1 {
            assert!(
                matches!(result.decision,IntakeDecision::ExistingWork{change} if change.id=="work-2")
            );
        } else {
            let IntakeDecision::ChoiceRequired { candidates } = result.decision else {
                panic!("expected native choices")
            };
            assert_eq!(candidates.len(), 2);
            assert!(
                candidates
                    .iter()
                    .all(|c| matches!(c, CandidateReference::NativeWork { .. }))
            );
        }
    }
}

fn corrupt_outcome(
    journal: &IssueJournal,
    id: &str,
    alter: impl FnOnce(&mut serde_json::Value),
    reseal: bool,
) {
    let connection = rusqlite::Connection::open(journal.database_path()).unwrap();
    let operation = journal.operation(id).unwrap().unwrap();
    let mut value = operation.outcome.clone().unwrap();
    alter(&mut value);
    let bytes = serde_json::to_vec(&value).unwrap();
    let checksum = if reseal {
        receipt_checksum(&operation, &bytes)
    } else {
        operation.outcome_digest
    };
    connection
        .execute(
            "UPDATE operations SET outcome_json=?2,outcome_digest=?3 WHERE id=?1",
            rusqlite::params![id, bytes, checksum],
        )
        .unwrap();
}

#[test]
fn operation_corruption_rejects_cross_target_and_fabricated_current_fence() {
    for intake_record in [false, true] {
        for reseal in [false, true] {
            for other_target in [false, true] {
                if intake_record && reseal && !other_target {
                    continue;
                }
                let (directory, native) = journal_fixture();
                let mut journal =
                    IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
                let found = observation(vec![issue(
                    41,
                    40,
                    IssueState::Open,
                    MatchBasis::ExactRequest,
                )]);
                let record = journal
                    .register("r", journal_request(&native), found.clone(), None)
                    .unwrap()
                    .outcome;
                let intent = selection(1, 41, None);
                journal
                    .acquire("a", &record.key, "worker", None, &intent)
                    .unwrap();
                journal
                    .mutate_claim(
                        "p",
                        &intent.issue,
                        "worker",
                        1,
                        ClaimAction::Progress {
                            digest: B.into(),
                            change: None,
                        },
                    )
                    .unwrap();
                let id = if intake_record { "r" } else { "a" };
                corrupt_outcome(
                    &journal,
                    id,
                    |value| {
                        if other_target {
                            if intake_record {
                                value["key"] = serde_json::json!(B);
                            } else {
                                value["issue"]["remote_id"] = serde_json::json!(42);
                            }
                        } else {
                            value["generation"] = serde_json::json!(2);
                        }
                    },
                    reseal,
                );
                assert!(journal.operation(id).is_err());
                if intake_record {
                    assert!(
                        journal
                            .register("r", journal_request(&native), found, None)
                            .is_err()
                    );
                } else {
                    assert!(
                        journal
                            .acquire("a", &record.key, "worker", None, &intent)
                            .is_err()
                    );
                }
                drop(journal);
                assert!(IssueJournal::open(directory.path(), &native, "installation-1").is_err());
            }
        }
    }
}

#[test]
fn operations_reject_orphans_wrong_kind_shape_and_null_outcome() {
    for corruption in 0..4 {
        let (directory, native) = journal_fixture();
        let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        let record = journal
            .register(
                "r",
                journal_request(&native),
                observation(vec![issue(
                    41,
                    40,
                    IssueState::Open,
                    MatchBasis::ExactRequest,
                )]),
                None,
            )
            .unwrap()
            .outcome;
        let intent = selection(1, 41, None);
        journal
            .acquire("a", &record.key, "worker", None, &intent)
            .unwrap();
        let mut operation = journal.operation("a").unwrap().unwrap();
        let connection = rusqlite::Connection::open(journal.database_path()).unwrap();
        let bytes: Vec<u8> = connection
            .query_row(
                "SELECT outcome_json FROM operations WHERE id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        match corruption {
            0 => {
                connection
                    .pragma_update(None, "foreign_keys", "OFF")
                    .unwrap();
                operation.intake_key = Some(B.into());
                connection
                    .execute(
                        "UPDATE operations SET intake_key=?1,outcome_digest=?2 WHERE id='a'",
                        rusqlite::params![B, receipt_checksum(&operation, &bytes)],
                    )
                    .unwrap();
            }
            1 => {
                // Both acquire selectors cannot masquerade as a progress action.
                connection
                    .pragma_update(None, "ignore_check_constraints", "ON")
                    .unwrap();
                operation.kind = "progress".into();
                connection
                    .execute(
                        "UPDATE operations SET kind='progress',outcome_digest=?1 WHERE id='a'",
                        [receipt_checksum(&operation, &bytes)],
                    )
                    .unwrap();
            }
            2 => {
                // Rebuild an altered schema to represent a null terminal outcome;
                // reopen and same-handle access must reject the table contract.
                let ddl: String = connection
                    .query_row(
                        "SELECT sql FROM sqlite_master WHERE name='operations'",
                        [],
                        |r| r.get(0),
                    )
                    .unwrap();
                connection
                    .pragma_update(None, "foreign_keys", "OFF")
                    .unwrap();
                connection
                    .execute_batch("ALTER TABLE operations RENAME TO old_operations")
                    .unwrap();
                connection
                    .execute_batch(&ddl.replace("outcome_json BLOB NOT NULL", "outcome_json BLOB"))
                    .unwrap();
                connection.execute_batch("INSERT INTO operations SELECT * FROM old_operations; DROP TABLE old_operations; UPDATE operations SET outcome_json=NULL WHERE id='a';").unwrap();
            }
            _ => {
                connection
                    .pragma_update(None, "ignore_check_constraints", "ON")
                    .unwrap();
                operation.intake_key = None;
                connection
                    .execute(
                        "UPDATE operations SET intake_key=NULL,outcome_digest=?1 WHERE id='a'",
                        [receipt_checksum(&operation, &bytes)],
                    )
                    .unwrap();
            }
        }
        drop(connection);
        assert!(journal.operation("a").is_err());
        assert!(
            journal
                .acquire("a", &record.key, "worker", None, &intent)
                .is_err()
        );
        drop(journal);
        assert!(IssueJournal::open(directory.path(), &native, "installation-1").is_err());
    }
}

#[test]
fn exact_schema_rejects_changed_table_trigger_index_and_view() {
    for change in 0..5 {
        let (directory, native) = journal_fixture();
        let journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        let connection = rusqlite::Connection::open(journal.database_path()).unwrap();
        match change {
            0 => {
                connection
                    .execute_batch(
                        "DROP TABLE operations; CREATE TABLE operations(id TEXT PRIMARY KEY)",
                    )
                    .unwrap();
            }
            1 => {
                connection
                    .execute_batch(
                        "CREATE TRIGGER extra_trigger AFTER UPDATE ON metadata BEGIN SELECT 1; END",
                    )
                    .unwrap();
            }
            2 => {
                connection
                    .execute_batch("CREATE INDEX extra_index ON claims(worker_id)")
                    .unwrap();
            }
            3 => {
                connection
                    .execute_batch("CREATE VIEW extra_view AS SELECT * FROM metadata")
                    .unwrap();
            }
            _ => {
                connection
                    .execute_batch("CREATE INDEX sqliteXevade ON claims(worker_id)")
                    .unwrap();
            }
        }
        drop(connection);
        assert!(journal.operation("unknown").is_err());
        drop(journal);
        assert!(IssueJournal::open(directory.path(), &native, "installation-1").is_err());
    }
}

#[test]
fn historical_claim_replay_remains_available_at_capacity() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let record = journal
        .register(
            "r",
            journal_request(&native),
            observation(vec![issue(
                41,
                40,
                IssueState::Open,
                MatchBasis::ExactRequest,
            )]),
            None,
        )
        .unwrap()
        .outcome;
    let intent = selection(1, 41, None);
    let acquired = journal
        .acquire("a", &record.key, "worker", None, &intent)
        .unwrap()
        .outcome;
    journal
        .mutate_claim("release", &intent.issue, "worker", 1, ClaimAction::Release)
        .unwrap();
    fill_operation_capacity(&journal, "r");
    let replay = journal
        .acquire("a", &record.key, "worker", None, &intent)
        .unwrap();
    assert!(replay.replay);
    assert!(!replay.current_fence);
    assert_eq!(replay.outcome, acquired);
    assert!(matches!(
        journal.acquire("new", &record.key, "next", Some(2), &selection(2, 41, None)),
        Err(Error::Limit(_))
    ));
    assert_eq!(journal.claim(&intent.issue).unwrap().unwrap().generation, 2);
}

#[test]
fn oversized_retained_record_blocks_prior_create_after_bounded_input() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let request = journal_request(&native);
    let observed = observation(
        (1..=40)
            .map(|id| issue(id, id, IssueState::Open, MatchBasis::ExactRequest))
            .collect(),
    );
    assert!(
        serde_json::to_vec(&(&request, &observed, &None::<OperatorChoice>))
            .unwrap()
            .len()
            < 16384
    );
    let outcome = evaluate(&request, observed.clone(), None).unwrap();
    let initial = journal
        .register("r", request.clone(), observation(vec![]), None)
        .unwrap()
        .outcome;
    let mut prospective = initial.clone();
    prospective.generation += 1;
    prospective.outcome = outcome;
    assert!(serde_json::to_vec(&prospective).unwrap().len() > 16384);
    let blocked = journal
        .reassess(
            "large-record",
            &initial.key,
            1,
            request.clone(),
            observed.clone(),
            None,
        )
        .unwrap()
        .outcome;
    assert!(blocked.assessment_conflicted);
    assert_eq!(blocked.generation, 2);
    assert_eq!(blocked.outcome, initial.outcome);
    assert_eq!(blocked.request, initial.request);
    assert!(journal.reserve_create("stale", &initial.key, 1).is_err());
    assert!(journal.reserve_create("current", &initial.key, 2).is_err());
    drop(journal);
    let mut reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    assert_eq!(reopened.intake(&initial.key).unwrap().unwrap(), blocked);
    assert!(
        reopened
            .reserve_create("reopened", &initial.key, 2)
            .is_err()
    );
    let (fresh_directory, fresh_native) = journal_fixture();
    let mut fresh =
        IssueJournal::open(fresh_directory.path(), &fresh_native, "installation-1").unwrap();
    assert!(matches!(
        fresh.register(
            "initial-large",
            journal_request(&fresh_native),
            observed,
            None
        ),
        Err(Error::Limit(_))
    ));
    let connection = rusqlite::Connection::open(fresh.database_path()).unwrap();
    let count:i64=connection.query_row("SELECT (SELECT count(*) FROM intakes)+(SELECT count(*) FROM claims)+(SELECT count(*) FROM operations)",[],|r|r.get(0)).unwrap();
    assert_eq!(count, 0);
    assert!(fresh.operation("initial-large").unwrap().is_none());
}

#[test]
fn initial_reconciliation_overflow_durably_blocks_prior_reservation() {
    const JSON_LIMIT: usize = 16 * 1024;
    let size = |record: &IntakeRecord| serde_json::to_vec(record).unwrap().len();
    let counts = |journal: &IssueJournal| {
        let connection = rusqlite::Connection::open(journal.database_path()).unwrap();
        connection.query_row("SELECT (SELECT count(*) FROM intakes),(SELECT count(*) FROM claims),(SELECT count(*) FROM operations)",[],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?))).unwrap()
    };
    for full in [false, true] {
        let (directory, native) = journal_fixture();
        let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        let request = journal_request(&native);
        request.validate().unwrap();
        let choice = Some(OperatorChoice::SeparateIntent {});
        let candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
        let mut with_proposal = candidate.clone();
        with_proposal.related_proposal = Some(ProposalIdentity {
            repository: repo(&format!("{}/{}", "a".repeat(256), "b".repeat(255))),
            remote_id: 9,
            number: 9,
        });
        let proposal = with_proposal.related_proposal.as_ref().unwrap();
        assert_eq!(proposal.repository.path.len(), 512);
        assert!(proposal.repository.same_repository(&request.repository));
        proposal.validate().unwrap();
        observation(vec![with_proposal.clone()]).validate().unwrap();
        // Search only the bounded duplicate count. Identical rows survive in
        // coverage, while SeparateIntent keeps the canonical decision eligible.
        let mut fixture = None;
        for count in 1..=MAX_CANDIDATES {
            let input = observation(vec![
                issue(1, 1, IssueState::Open, MatchBasis::Similarity);
                count
            ]);
            input.validate().unwrap();
            let outcome = evaluate(&request, input.clone(), choice.as_ref()).unwrap();
            assert_eq!(outcome.decision, IntakeDecision::CreateEligible {});
            let mut projected = IntakeRecord {
                key: A.into(),
                request: request.clone(),
                choice: choice.clone(),
                outcome,
                generation: 3,
                create_state: CreateState::Observed,
                create_operation: Some("create".into()),
                selected_issue: Some(candidate.identity.clone()),
                reconciled_observation: Some(candidate.clone()),
                claimed_issue: None,
                assessment_conflicted: false,
            };
            let mut large = projected.clone();
            large.reconciled_observation = Some(with_proposal.clone());
            // Account for the subsequent acquire's sticky issue AND generation,
            // so an unrelated acquire-size failure cannot mask the old bypass.
            projected.claimed_issue = Some(candidate.identity.clone());
            projected.generation += 1;
            if size(&projected) <= JSON_LIMIT && size(&large) > JSON_LIMIT {
                fixture = Some(input);
                break;
            }
        }
        let input =
            fixture.expect("a legal duplicate-count fixture must straddle the retained JSON limit");
        assert!(input.open_issues.candidates().len() <= MAX_CANDIDATES);
        assert!(
            input
                .open_issues
                .candidates()
                .iter()
                .all(|row| row == &input.open_issues.candidates()[0])
        );
        assert!(serde_json::to_vec(&input).unwrap().len() <= MAX_INTAKE_BYTES);
        let registered = journal
            .register("r", request.clone(), input.clone(), choice.clone())
            .unwrap()
            .outcome;
        assert!(registered.outcome.trace.coverage_complete);
        assert_eq!(registered.outcome.trace.candidate_keys.len(), 1);
        assert_eq!(registered.outcome.coverage, input);
        let reserved = journal
            .reserve_create("create", &registered.key, registered.generation)
            .unwrap()
            .outcome;
        let registration_history = journal.operation("r").unwrap().unwrap();
        let reservation_history = journal.operation("create").unwrap().unwrap();
        // Reassert against the real key and exact saved reservation, rather than
        // relying on the sizing scaffold's fixed-length placeholder key.
        let mut no_proposal = reserved.clone();
        no_proposal.generation += 1;
        no_proposal.create_state = CreateState::Observed;
        no_proposal.selected_issue = Some(candidate.identity.clone());
        no_proposal.reconciled_observation = Some(candidate.clone());
        let mut large = no_proposal.clone();
        large.reconciled_observation = Some(with_proposal.clone());
        let mut acquired_projection = no_proposal.clone();
        acquired_projection.claimed_issue = Some(candidate.identity.clone());
        acquired_projection.generation += 1;
        assert!(size(&registered) <= JSON_LIMIT);
        assert!(size(&reserved) <= JSON_LIMIT);
        assert!(size(&no_proposal) <= JSON_LIMIT);
        assert!(size(&acquired_projection) <= JSON_LIMIT);
        assert!(size(&large) > JSON_LIMIT);
        if full {
            fill_operation_capacity(&journal, "r");
        }
        let prior_counts = counts(&journal);
        assert_eq!(prior_counts.1, 0);
        if full {
            assert_eq!(prior_counts.0 + prior_counts.1 + prior_counts.2, 4096);
        }
        assert!(matches!(
            journal.reconcile_create(
                "create",
                &registered.key,
                candidate.identity.clone(),
                with_proposal.clone()
            ),
            Err(Error::Limit(_))
        ));
        let mut blocked = reserved.clone();
        blocked.assessment_conflicted = true;
        blocked.generation += 1;
        assert_eq!(journal.intake(&registered.key).unwrap().unwrap(), blocked);
        assert_eq!(counts(&journal), prior_counts);
        assert_eq!(
            journal.operation("r").unwrap().unwrap(),
            registration_history
        );
        assert_eq!(
            journal.operation("create").unwrap().unwrap(),
            reservation_history
        );
        drop(journal);
        let mut reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        assert_eq!(reopened.intake(&registered.key).unwrap().unwrap(), blocked);
        assert!(matches!(
            reopened.reconcile_create(
                "create",
                &registered.key,
                candidate.identity.clone(),
                with_proposal
            ),
            Err(Error::Limit(_))
        ));
        assert_eq!(reopened.intake(&registered.key).unwrap().unwrap(), blocked);
        assert_eq!(counts(&reopened), prior_counts);
        assert!(matches!(
            reopened.reserve_create("recreate-reserved", &registered.key, blocked.generation),
            Err(Error::Conflict(_))
        ));
        let observed = reopened
            .reconcile_create(
                "create",
                &registered.key,
                candidate.identity.clone(),
                candidate.clone(),
            )
            .unwrap();
        let mut expected = no_proposal;
        expected.assessment_conflicted = true;
        expected.generation = blocked.generation + 1;
        assert_eq!(observed, expected);
        assert_eq!(
            reopened
                .reconcile_create(
                    "create",
                    &registered.key,
                    candidate.identity.clone(),
                    candidate.clone()
                )
                .unwrap(),
            observed
        );
        drop(reopened);
        let mut reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        assert_eq!(reopened.intake(&registered.key).unwrap().unwrap(), observed);
        // Both fences must be denied by the evidence barrier, including at cap.
        for (id, generation) in [
            ("stale", reserved.generation + 1),
            ("reloaded", observed.generation),
        ] {
            assert!(
                matches!(reopened.acquire(id,&registered.key,"worker",None,&selection(generation,41,None)),Err(Error::Conflict(message)) if message=="intake requires reassessment")
            );
            assert!(reopened.operation(id).unwrap().is_none());
        }
        assert!(matches!(
            reopened.reserve_create("recreate-observed", &registered.key, observed.generation),
            Err(Error::Conflict(_))
        ));
        assert!(reopened.claim(&candidate.identity).unwrap().is_none());
        assert_eq!(counts(&reopened), prior_counts);
        assert_eq!(
            reopened.operation("r").unwrap().unwrap(),
            registration_history
        );
        assert_eq!(
            reopened.operation("create").unwrap().unwrap(),
            reservation_history
        );
        let replay = reopened
            .register("r", request.clone(), input, choice)
            .unwrap();
        assert!(replay.replay);
        assert_eq!(replay.outcome, registered);
        let replay = reopened
            .reserve_create("create", &registered.key, registered.generation)
            .unwrap();
        assert!(replay.replay);
        assert_eq!(replay.outcome, reserved);
        assert_eq!(reopened.intake(&registered.key).unwrap().unwrap(), observed);
        let recovery = reopened.reassess(
            "recover",
            &registered.key,
            observed.generation,
            request,
            observation(vec![candidate.clone()]),
            None,
        );
        if full {
            assert!(matches!(recovery, Err(Error::Limit(_))));
            assert!(reopened.operation("recover").unwrap().is_none());
            assert_eq!(reopened.intake(&registered.key).unwrap().unwrap(), observed);
            assert_eq!(counts(&reopened), prior_counts);
        } else {
            let recovered = recovery.unwrap().outcome;
            assert!(!recovered.assessment_conflicted);
            assert_eq!(recovered.generation, observed.generation + 1);
            assert_eq!(recovered.selected_issue, observed.selected_issue);
            assert_eq!(
                recovered.reconciled_observation,
                observed.reconciled_observation
            );
            let owned = reopened
                .acquire(
                    "recovered",
                    &registered.key,
                    "worker",
                    None,
                    &selection(recovered.generation, 41, None),
                )
                .unwrap();
            assert!(owned.current_fence);
            assert_eq!(owned.outcome.issue, candidate.identity);
            assert!(
                reopened
                    .reserve_create("never-recreate", &registered.key, recovered.generation)
                    .is_err()
            );
        }
        assert_eq!(
            reopened.operation("r").unwrap().unwrap(),
            registration_history
        );
        assert_eq!(
            reopened.operation("create").unwrap().unwrap(),
            reservation_history
        );
    }
}

#[test]
fn negative_same_issue_refresh_is_durable_and_historical_after_recovery() {
    for observed_create in [false, true] {
        let (directory, native) = journal_fixture();
        let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        let candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
        let found = observation(vec![candidate.clone()]);
        let initial = journal
            .register(
                "r",
                journal_request(&native),
                if observed_create {
                    observation(vec![])
                } else {
                    found.clone()
                },
                None,
            )
            .unwrap()
            .outcome;
        if observed_create {
            journal.reserve_create("create", &initial.key, 1).unwrap();
            journal
                .reconcile_create(
                    "create",
                    &initial.key,
                    candidate.identity.clone(),
                    candidate.clone(),
                )
                .unwrap();
        }
        let ready = journal.intake(&initial.key).unwrap().unwrap();
        journal
            .acquire(
                "a",
                &initial.key,
                "worker",
                None,
                &selection(ready.generation, 41, None),
            )
            .unwrap();
        let change = ChangeId {
            repository: native.clone(),
            id: "known".into(),
        };
        journal
            .mutate_claim(
                "p",
                &candidate.identity,
                "worker",
                1,
                ClaimAction::Progress {
                    digest: B.into(),
                    change: Some(change.clone()),
                },
            )
            .unwrap();
        journal
            .mutate_claim(
                "release",
                &candidate.identity,
                "worker",
                2,
                ClaimAction::Release,
            )
            .unwrap();
        let mut generation = journal.intake(&initial.key).unwrap().unwrap().generation;
        let mut closed = observation(vec![]);
        closed.closed_issues = coverage(vec![issue(
            41,
            40,
            IssueState::Closed,
            MatchBasis::ExactRequest,
        )]);
        let mut proposal = found.clone();
        if let Coverage::Complete { candidates } = &mut proposal.open_issues {
            candidates[0].related_proposal = Some(ProposalIdentity {
                repository: repo("owner/project"),
                remote_id: 9,
                number: 9,
            });
        }
        let mut incomplete = found.clone();
        incomplete.proposals = Coverage::Incomplete {
            reason: IncompleteReason::MorePages,
            candidates: vec![],
        };
        let negatives = vec![
            observation(vec![]),
            observation(vec![issue(
                42,
                42,
                IssueState::Open,
                MatchBasis::ExactRequest,
            )]),
            closed,
            proposal,
            incomplete,
            native_observation(&native, "different"),
        ];
        let mut historical = None;
        for (n, negative) in negatives.into_iter().enumerate() {
            let prior = journal.intake(&initial.key).unwrap().unwrap();
            let operation = format!("negative-{n}");
            let blocked = journal
                .reassess(
                    &operation,
                    &initial.key,
                    generation,
                    journal_request(&native),
                    negative.clone(),
                    None,
                )
                .unwrap();
            assert!(blocked.outcome.assessment_conflicted);
            assert!(!blocked.current_fence);
            assert!(!blocked.replay);
            assert_eq!(blocked.outcome.generation, generation + 1);
            assert_eq!(blocked.outcome.outcome, prior.outcome);
            assert_eq!(blocked.outcome.request, prior.request);
            assert_eq!(
                blocked.outcome.reconciled_observation,
                prior.reconciled_observation
            );
            assert!(
                journal
                    .acquire(
                        &format!("blocked-{n}"),
                        &initial.key,
                        "next",
                        Some(3),
                        &selection(generation + 1, 41, None)
                    )
                    .is_err()
            );
            historical = Some((operation, generation, negative, blocked.outcome.clone()));
            generation += 1;
        }
        let claim_before = journal.claim(&candidate.identity).unwrap().unwrap();
        let recovered = journal
            .reassess(
                "recover",
                &initial.key,
                generation,
                journal_request(&native),
                native_observation(&native, "known"),
                None,
            )
            .unwrap()
            .outcome;
        assert!(!recovered.assessment_conflicted);
        assert_eq!(
            journal.claim(&candidate.identity).unwrap().unwrap(),
            claim_before
        );
        let (id, expected, negative, old) = historical.unwrap();
        let replay = journal
            .reassess(
                &id,
                &initial.key,
                expected,
                journal_request(&native),
                negative,
                None,
            )
            .unwrap();
        assert!(replay.replay);
        assert_eq!(replay.outcome, old);
        assert_eq!(journal.intake(&initial.key).unwrap().unwrap(), recovered);
        // Missing current Change evidence preserves the canonical association.
        let fresh = journal
            .reassess(
                "reuse",
                &initial.key,
                recovered.generation,
                journal_request(&native),
                found,
                None,
            )
            .unwrap()
            .outcome;
        let acquired = journal
            .acquire(
                "again",
                &initial.key,
                "next",
                Some(3),
                &selection(fresh.generation, 41, None),
            )
            .unwrap();
        assert_eq!(acquired.outcome.change, Some(change));
        assert_eq!(acquired.outcome.progress_digest.as_deref(), Some(B));
        assert!(
            journal
                .reserve_create("never-recreate", &initial.key, fresh.generation)
                .is_err()
        );
        drop(journal);
        let reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        assert_eq!(reopened.intake(&initial.key).unwrap().unwrap(), fresh);
    }
}

#[test]
fn direct_observed_negative_refresh_and_capacity_invalidation() {
    for full in [false, true] {
        let (directory, native) = journal_fixture();
        let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        let initial = journal
            .register("r", journal_request(&native), observation(vec![]), None)
            .unwrap()
            .outcome;
        journal.reserve_create("create", &initial.key, 1).unwrap();
        let candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
        let ready = journal
            .reconcile_create(
                "create",
                &initial.key,
                candidate.identity.clone(),
                candidate.clone(),
            )
            .unwrap();
        if full {
            fill_operation_capacity(&journal, "r");
        }
        let negative = journal.reassess(
            "negative",
            &initial.key,
            ready.generation,
            journal_request(&native),
            observation(vec![]),
            None,
        );
        if full {
            assert!(matches!(negative, Err(Error::Limit(_))));
            assert!(journal.operation("negative").unwrap().is_none());
        } else {
            assert!(negative.unwrap().outcome.assessment_conflicted);
        }
        let blocked = journal.intake(&initial.key).unwrap().unwrap();
        assert!(blocked.assessment_conflicted);
        assert_eq!(blocked.generation, ready.generation + 1);
        assert!(
            journal
                .acquire(
                    "blocked",
                    &initial.key,
                    "w",
                    None,
                    &selection(blocked.generation, 41, None)
                )
                .is_err()
        );
        if full {
            assert!(matches!(
                journal.reassess(
                    "again",
                    &initial.key,
                    blocked.generation,
                    journal_request(&native),
                    observation(vec![]),
                    None
                ),
                Err(Error::Limit(_))
            ));
            assert_eq!(journal.intake(&initial.key).unwrap().unwrap(), blocked);
            assert!(matches!(
                journal.reassess(
                    "positive",
                    &initial.key,
                    blocked.generation,
                    journal_request(&native),
                    observation(vec![candidate]),
                    None
                ),
                Err(Error::Limit(_))
            ));
            assert_eq!(journal.intake(&initial.key).unwrap().unwrap(), blocked);
            let history = journal.operation("create").unwrap().unwrap();
            let registration = journal.operation("r").unwrap().unwrap();
            drop(journal);
            let mut reopened =
                IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
            assert_eq!(reopened.intake(&initial.key).unwrap().unwrap(), blocked);
            assert_eq!(reopened.operation("create").unwrap().unwrap(), history);
            assert_eq!(reopened.operation("r").unwrap().unwrap(), registration);
            assert!(reopened.operation("negative").unwrap().is_none());
            assert!(
                reopened
                    .acquire(
                        "reopen-blocked",
                        &initial.key,
                        "w",
                        None,
                        &selection(blocked.generation, 41, None)
                    )
                    .is_err()
            );
            assert!(
                reopened
                    .reserve_create("reopen-create", &initial.key, blocked.generation)
                    .is_err()
            );
        }
    }
}

#[test]
fn reconcile_display_retry_and_material_contradictions_preserve_history() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let initial = journal
        .register("r", journal_request(&native), observation(vec![]), None)
        .unwrap()
        .outcome;
    journal.reserve_create("create", &initial.key, 1).unwrap();
    let mut closed = issue(41, 40, IssueState::Closed, MatchBasis::ExactRequest);
    assert!(
        journal
            .reconcile_create(
                "wrong",
                &initial.key,
                closed.identity.clone(),
                closed.clone()
            )
            .is_err()
    );
    assert_eq!(journal.intake(&initial.key).unwrap().unwrap().generation, 2);
    assert!(
        journal
            .reconcile_create(
                "create",
                &initial.key,
                closed.identity.clone(),
                closed.clone()
            )
            .is_err()
    );
    let blocked = journal.intake(&initial.key).unwrap().unwrap();
    assert!(blocked.assessment_conflicted);
    assert_eq!(blocked.generation, 3);
    assert_eq!(blocked.create_state, CreateState::ReservedUncertain);
    closed.state = IssueState::Open;
    let observed = journal
        .reconcile_create(
            "create",
            &initial.key,
            closed.identity.clone(),
            closed.clone(),
        )
        .unwrap();
    assert!(observed.assessment_conflicted);
    let mut display = closed.clone();
    display.identity.repository.path = "renamed/project".into();
    display.identity.number = 400;
    assert_eq!(
        journal
            .reconcile_create(
                "create",
                &initial.key,
                display.identity.clone(),
                display.clone()
            )
            .unwrap(),
        observed
    );
    let refreshed = journal
        .reassess(
            "refresh",
            &initial.key,
            observed.generation,
            journal_request(&native),
            observation(vec![display.clone()]),
            None,
        )
        .unwrap()
        .outcome;
    fill_operation_capacity(&journal, "r");
    display.related_proposal = Some(ProposalIdentity {
        repository: repo("owner/project"),
        remote_id: 9,
        number: 9,
    });
    assert!(
        journal
            .reconcile_create(
                "create",
                &initial.key,
                display.identity.clone(),
                display.clone()
            )
            .is_err()
    );
    let conflicted = journal.intake(&initial.key).unwrap().unwrap();
    assert!(conflicted.assessment_conflicted);
    assert_eq!(conflicted.generation, refreshed.generation + 1);
    assert_eq!(
        conflicted.reconciled_observation,
        observed.reconciled_observation
    );
    assert!(
        journal
            .reconcile_create("create", &initial.key, display.identity.clone(), display)
            .is_err()
    );
    assert_eq!(journal.intake(&initial.key).unwrap().unwrap(), conflicted);
    let other = issue(42, 42, IssueState::Open, MatchBasis::ExactRequest);
    assert!(
        journal
            .reconcile_create("create", &initial.key, other.identity.clone(), other)
            .is_err()
    );
    assert_eq!(journal.intake(&initial.key).unwrap().unwrap(), conflicted);
}

#[test]
fn request_display_refresh_preserves_exact_history_and_active_claim_display() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let original = journal_request(&native);
    let candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
    let found = observation(vec![candidate.clone()]);
    let registered = journal
        .register("r", original.clone(), found.clone(), None)
        .unwrap()
        .outcome;
    let intent = selection(1, 41, None);
    let owned = journal
        .acquire("a", &registered.key, "worker", None, &intent)
        .unwrap()
        .outcome;
    let mut request = original.clone();
    request.repository.path = "renamed/project".into();
    let mut renamed = found.clone();
    renamed.repository = request.repository.clone();
    if let Coverage::Complete { candidates } = &mut renamed.open_issues {
        candidates[0].identity.repository = request.repository.clone();
        candidates[0].identity.number = 400;
    }
    let refreshed = journal
        .reassess(
            "rename",
            &registered.key,
            2,
            request.clone(),
            renamed.clone(),
            None,
        )
        .unwrap()
        .outcome;
    assert_eq!(journal.claim(&candidate.identity).unwrap().unwrap(), owned);
    assert_eq!(
        refreshed.claimed_issue.as_ref().unwrap().repository.path,
        "renamed/project"
    );
    assert_eq!(
        journal
            .register("truthful", request.clone(), renamed.clone(), None)
            .unwrap()
            .outcome,
        refreshed
    );
    let mut wrong = request.clone();
    wrong.repository.remote_id = 8;
    assert!(
        journal
            .reassess(
                "wrong",
                &registered.key,
                refreshed.generation,
                wrong,
                renamed.clone(),
                None
            )
            .is_err()
    );
    assert_eq!(journal.intake(&registered.key).unwrap().unwrap(), refreshed);
    let mut wrong_native = request.clone();
    wrong_native.native_repository = if native == NATIVE {
        "dddddddddddddddddddddddddddddddd".into()
    } else {
        NATIVE.into()
    };
    assert!(
        journal
            .reassess(
                "wrong-native",
                &registered.key,
                refreshed.generation,
                wrong_native,
                renamed.clone(),
                None
            )
            .is_err()
    );
    let mut wrong_digest = request.clone();
    wrong_digest.accepted_request_sha256 = B.into();
    assert!(
        journal
            .reassess(
                "wrong-digest",
                &registered.key,
                refreshed.generation,
                wrong_digest,
                renamed.clone(),
                None
            )
            .is_err()
    );
    journal
        .mutate_claim(
            "release",
            &candidate.identity,
            "worker",
            1,
            ClaimAction::Release,
        )
        .unwrap();
    let reacquired = journal
        .acquire(
            "again",
            &registered.key,
            "next",
            Some(2),
            &selection(refreshed.generation, 41, None),
        )
        .unwrap();
    assert_eq!(reacquired.outcome.issue.number, 400);
    drop(journal);
    let mut reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    assert_eq!(
        reopened
            .register("r", original, found, None)
            .unwrap()
            .outcome,
        registered
    );
    assert_eq!(
        reopened
            .acquire("a", &registered.key, "worker", None, &intent)
            .unwrap()
            .outcome,
        owned
    );
    let replay = reopened
        .reassess("rename", &registered.key, 2, request, renamed, None)
        .unwrap();
    assert!(replay.replay);
    assert_eq!(replay.outcome, refreshed);
}

#[test]
fn fresh_same_issue_proof_supersedes_historical_proposal_without_rewriting_it() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let initial = journal
        .register("r", journal_request(&native), observation(vec![]), None)
        .unwrap()
        .outcome;
    let reservation = journal
        .reserve_create("create", &initial.key, 1)
        .unwrap()
        .outcome;
    let mut candidate = issue(41, 40, IssueState::Open, MatchBasis::ExactRequest);
    candidate.related_proposal = Some(ProposalIdentity {
        repository: repo("owner/project"),
        remote_id: 9,
        number: 9,
    });
    let observed = journal
        .reconcile_create(
            "create",
            &initial.key,
            candidate.identity.clone(),
            candidate.clone(),
        )
        .unwrap();
    assert!(
        journal
            .acquire(
                "baseline",
                &initial.key,
                "w",
                None,
                &selection(observed.generation, 41, None)
            )
            .is_err()
    );
    let mut display = candidate.clone();
    display.identity.number = 400;
    display.related_proposal.as_mut().unwrap().number = 900;
    display.related_proposal.as_mut().unwrap().repository.path = "renamed/project".into();
    assert_eq!(
        journal
            .reconcile_create("create", &initial.key, display.identity.clone(), display)
            .unwrap(),
        observed
    );
    let current = journal
        .reassess(
            "fresh",
            &initial.key,
            observed.generation,
            journal_request(&native),
            native_observation(&native, "known"),
            None,
        )
        .unwrap()
        .outcome;
    assert_eq!(
        current.reconciled_observation,
        observed.reconciled_observation
    );
    assert_eq!(current.selected_issue, observed.selected_issue);
    let change = ChangeId {
        repository: native.clone(),
        id: "known".into(),
    };
    journal
        .acquire(
            "work",
            &initial.key,
            "w",
            None,
            &selection(current.generation, 41, Some(change)),
        )
        .unwrap();
    let after = journal.intake(&initial.key).unwrap().unwrap();
    // Invalid evidence and stale/reused IDs cannot disturb current readiness.
    let mut malformed = observation(vec![]);
    malformed.version = 99;
    assert!(
        journal
            .reassess(
                "bad",
                &initial.key,
                after.generation,
                journal_request(&native),
                malformed,
                None
            )
            .is_err()
    );
    assert!(
        journal
            .reassess(
                "stale",
                &initial.key,
                observed.generation,
                journal_request(&native),
                observation(vec![]),
                None
            )
            .is_err()
    );
    assert!(
        journal
            .reassess(
                "fresh",
                &initial.key,
                observed.generation,
                journal_request(&native),
                observation(vec![]),
                None
            )
            .is_err()
    );
    assert_eq!(journal.intake(&initial.key).unwrap().unwrap(), after);
    assert_eq!(
        journal
            .reserve_create("create", &initial.key, 1)
            .unwrap()
            .outcome,
        reservation
    );
    drop(journal);
    let reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    assert_eq!(reopened.intake(&initial.key).unwrap().unwrap(), after);
}

#[test]
fn large_valid_evidence_blocks_released_sticky_claims_and_replays_after_recovery() {
    for register in [false, true] {
        let (directory, native) = journal_fixture();
        let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        let request = journal_request(&native);
        let found = observation(vec![issue(
            41,
            40,
            IssueState::Open,
            MatchBasis::ExactRequest,
        )]);
        let initial = journal
            .register("r", request.clone(), found.clone(), None)
            .unwrap()
            .outcome;
        journal
            .acquire("a", &initial.key, "worker", None, &selection(1, 41, None))
            .unwrap();
        journal
            .mutate_claim(
                "release",
                &selection(1, 41, None).issue,
                "worker",
                1,
                ClaimAction::Release,
            )
            .unwrap();
        let ready = journal.intake(&initial.key).unwrap().unwrap();
        let large = observation(
            (1..=100)
                .map(|id| issue(id, id, IssueState::Open, MatchBasis::ExactRequest))
                .collect(),
        );
        let args = serde_json::to_vec(&(&request, &large, &None::<OperatorChoice>)).unwrap();
        assert!(args.len() > 16384);
        assert!(args.len() < 256 * 1024);
        evaluate(&request, large.clone(), None).unwrap();
        if register {
            assert!(matches!(
                journal.register("large", request.clone(), large.clone(), None),
                Err(Error::Conflict(_))
            ));
        } else {
            assert!(
                journal
                    .reassess(
                        "large",
                        &initial.key,
                        ready.generation,
                        request.clone(),
                        large.clone(),
                        None
                    )
                    .unwrap()
                    .outcome
                    .assessment_conflicted
            );
        }
        let blocked = journal.intake(&initial.key).unwrap().unwrap();
        assert!(blocked.assessment_conflicted);
        assert_eq!(blocked.generation, ready.generation + 1);
        assert_eq!(blocked.outcome, ready.outcome);
        assert_eq!(blocked.claimed_issue, ready.claimed_issue);
        assert!(
            journal
                .acquire(
                    "stale",
                    &initial.key,
                    "next",
                    Some(2),
                    &selection(ready.generation, 41, None)
                )
                .is_err()
        );
        assert!(
            journal
                .acquire(
                    "reload",
                    &initial.key,
                    "next",
                    Some(2),
                    &selection(blocked.generation, 41, None)
                )
                .is_err()
        );
        assert!(
            journal
                .reserve_create("blocked-create", &initial.key, blocked.generation)
                .is_err()
        );
        drop(journal);
        let mut reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
        assert_eq!(reopened.intake(&initial.key).unwrap().unwrap(), blocked);
        assert!(
            reopened
                .acquire(
                    "reopen",
                    &initial.key,
                    "next",
                    Some(2),
                    &selection(blocked.generation, 41, None)
                )
                .is_err()
        );
        let recovered = reopened
            .reassess(
                "recover",
                &initial.key,
                blocked.generation,
                request.clone(),
                found,
                None,
            )
            .unwrap()
            .outcome;
        if register {
            assert!(matches!(
                reopened.register("large", request, large, None),
                Err(Error::Conflict(_))
            ));
        } else {
            let replay = reopened
                .reassess(
                    "large",
                    &initial.key,
                    ready.generation,
                    request,
                    large,
                    None,
                )
                .unwrap();
            assert!(replay.replay);
            assert_eq!(replay.outcome, blocked);
        }
        assert_eq!(reopened.intake(&initial.key).unwrap().unwrap(), recovered);
    }
}

#[test]
fn exhausted_released_sticky_intake_denies_new_acquire_but_preserves_history() {
    let (directory, native) = journal_fixture();
    let mut journal = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    let initial = journal
        .register(
            "r",
            journal_request(&native),
            observation(vec![issue(
                41,
                40,
                IssueState::Open,
                MatchBasis::ExactRequest,
            )]),
            None,
        )
        .unwrap()
        .outcome;
    let intent = selection(1, 41, None);
    let original = journal
        .acquire("a", &initial.key, "worker", None, &intent)
        .unwrap()
        .outcome;
    journal
        .mutate_claim("release", &intent.issue, "worker", 1, ClaimAction::Release)
        .unwrap();
    // Accepted stored boundary, unreachable through the ordinary 4,096-row profile.
    let mut exhausted = journal.intake(&initial.key).unwrap().unwrap();
    exhausted.generation = i64::MAX - 1;
    let connection = rusqlite::Connection::open(journal.database_path()).unwrap();
    connection
        .execute(
            "UPDATE intakes SET generation=?1,record_json=?2 WHERE key=?3",
            rusqlite::params![
                exhausted.generation,
                serde_json::to_vec(&exhausted).unwrap(),
                exhausted.key
            ],
        )
        .unwrap();
    drop(connection);
    drop(journal);
    let mut reopened = IssueJournal::open(directory.path(), &native, "installation-1").unwrap();
    assert!(matches!(
        reopened.reassess(
            "negative",
            &initial.key,
            exhausted.generation,
            journal_request(&native),
            observation(vec![]),
            None
        ),
        Err(Error::Limit(_))
    ));
    assert_eq!(reopened.intake(&initial.key).unwrap().unwrap(), exhausted);
    assert!(reopened.operation("negative").unwrap().is_none());
    let replay = reopened
        .acquire("a", &initial.key, "worker", None, &intent)
        .unwrap();
    assert!(replay.replay);
    assert!(!replay.current_fence);
    assert_eq!(replay.outcome, original);
    assert!(matches!(
        reopened.acquire(
            "new",
            &initial.key,
            "next",
            Some(2),
            &selection(exhausted.generation, 41, None)
        ),
        Err(Error::Limit(_))
    ));
    assert!(reopened.operation("new").unwrap().is_none());
    assert_eq!(
        reopened.claim(&intent.issue).unwrap().unwrap().generation,
        2
    );
}
