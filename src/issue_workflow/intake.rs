use std::collections::BTreeMap;

use crate::{Error, Result};

use super::{
    CandidateReference, IntakeDecision, IntakeObservation, IntakeOutcome, IntakeRequest,
    IntakeTrace, IssueCandidate, IssueState, MatchBasis, NativeWorkCandidate, OperatorChoice,
    ProposalCandidate, require_repository,
};

type Candidates = BTreeMap<String, (CandidateReference, IntakeDecision)>;

pub fn evaluate(
    request: &IntakeRequest,
    observation: IntakeObservation,
    choice: Option<&OperatorChoice>,
) -> Result<IntakeOutcome> {
    request.validate()?;
    observation.validate()?;
    if let Some(OperatorChoice::Select { candidate }) = choice {
        match candidate {
            CandidateReference::Issue { identity, .. } => identity.validate()?,
            CandidateReference::Proposal { identity } => identity.validate()?,
            CandidateReference::NativeWork { issue, change } => {
                issue.validate()?;
                change.validate()?;
            }
        }
    }
    require_repository(&request.repository, &observation.repository)?;
    for work in observation.native_work.candidates() {
        if work.change.repository != request.native_repository {
            return Err(Error::Conflict("native work repository mismatch".into()));
        }
    }

    validate_nested_identities(&observation)?;
    let issues = canonical_issues(&observation)?;
    let proposals = canonical_proposals(&observation)?;
    let work = canonical_work(&observation)?;
    let complete = observation.complete();
    let exact = exact_matches(&request.accepted_request_sha256, &issues, &proposals, &work);
    let base = if !complete {
        IntakeDecision::SearchIncomplete {}
    } else if exact.len() > 1 {
        IntakeDecision::ChoiceRequired {
            candidates: exact
                .values()
                .map(|(reference, _)| reference.clone())
                .collect(),
        }
    } else if let Some((_, decision)) = exact.values().next() {
        decision.clone()
    } else {
        let similar = similarity_matches(&issues, &proposals, &work);
        if similar.is_empty() {
            IntakeDecision::CreateEligible {}
        } else {
            IntakeDecision::ChoiceRequired {
                candidates: similar
                    .values()
                    .map(|(reference, _)| reference.clone())
                    .collect(),
            }
        }
    };

    let decision = match choice {
        None => base,
        Some(OperatorChoice::SeparateIntent {}) if complete && exact.is_empty() => {
            if matches!(base, IntakeDecision::ChoiceRequired { .. }) {
                IntakeDecision::CreateEligible {}
            } else {
                return Err(Error::Conflict(
                    "no similarity choice is currently required".into(),
                ));
            }
        }
        Some(OperatorChoice::Select { candidate }) => {
            let IntakeDecision::ChoiceRequired { candidates } = &base else {
                return Err(Error::Conflict(
                    "no candidate choice is currently required".into(),
                ));
            };
            let key = reference_key(candidate);
            let offered = candidates
                .iter()
                .any(|reference| reference_key(reference) == key);
            if !offered {
                return Err(Error::Conflict(
                    "selected candidate is not currently offered".into(),
                ));
            }
            let current = if exact.is_empty() {
                similarity_matches(&issues, &proposals, &work)
            } else {
                exact
            };
            let (reference, decision) =
                current.get(&key).expect("offered candidate has a decision");
            if reference != candidate {
                return Err(Error::Conflict(
                    "selected candidate metadata is stale".into(),
                ));
            }
            decision.clone()
        }
        Some(OperatorChoice::SeparateIntent {}) => {
            return Err(Error::Conflict(
                "exact or incomplete results cannot be dismissed".into(),
            ));
        }
    };

    let mut candidate_keys = issues.keys().cloned().collect::<Vec<_>>();
    candidate_keys.extend(proposals.keys().cloned());
    candidate_keys.extend(work.values().map(|candidate| {
        format!(
            "native:{}:{}",
            candidate.change.repository, candidate.change.id
        )
    }));
    candidate_keys.sort();
    candidate_keys.dedup();
    Ok(IntakeOutcome {
        decision,
        trace: IntakeTrace {
            repository_key: request.repository.stable_key(),
            coverage_complete: complete,
            candidate_keys,
        },
        coverage: observation,
    })
}

fn canonical_issues(observation: &IntakeObservation) -> Result<BTreeMap<String, &IssueCandidate>> {
    let mut found = BTreeMap::new();
    for candidate in observation
        .open_issues
        .candidates()
        .iter()
        .chain(observation.closed_issues.candidates())
    {
        let key = candidate.identity.stable_key();
        if let Some(previous) = found.insert(key, candidate)
            && previous != candidate
        {
            return Err(Error::Conflict(
                "same issue identity has contradictory metadata".into(),
            ));
        }
    }
    Ok(found)
}

fn canonical_proposals(
    observation: &IntakeObservation,
) -> Result<BTreeMap<String, &ProposalCandidate>> {
    let mut found = BTreeMap::new();
    for candidate in observation.proposals.candidates() {
        let key = candidate.identity.stable_key();
        if let Some(previous) = found.insert(key, candidate)
            && previous != candidate
        {
            return Err(Error::Conflict(
                "same proposal identity has contradictory metadata".into(),
            ));
        }
    }
    Ok(found)
}

fn canonical_work(
    observation: &IntakeObservation,
) -> Result<BTreeMap<String, &NativeWorkCandidate>> {
    let mut found = BTreeMap::new();
    let mut changes = BTreeMap::new();
    for candidate in observation.native_work.candidates() {
        let key = candidate.issue.stable_key();
        if let Some(previous) = found.insert(key, candidate)
            && previous != candidate
        {
            return Err(Error::Conflict(
                "same issue has contradictory native work".into(),
            ));
        }
        let change_key = (&candidate.change.repository, &candidate.change.id);
        if let Some(previous) = changes.insert(change_key, candidate)
            && previous != candidate
        {
            return Err(Error::Conflict(
                "same native change has contradictory metadata".into(),
            ));
        }
    }
    Ok(found)
}

fn reference_key(reference: &CandidateReference) -> String {
    match reference {
        CandidateReference::Issue { identity, .. } => identity.stable_key(),
        CandidateReference::Proposal { identity } => identity.stable_key(),
        CandidateReference::NativeWork { change, .. } => {
            format!("native:{}:{}", change.repository, change.id)
        }
    }
}

fn insert_issue(
    matches: &mut Candidates,
    issue: &IssueCandidate,
    work: &BTreeMap<String, &NativeWorkCandidate>,
) {
    let issue_key = issue.identity.stable_key();
    let pair = if let Some(work) = work.get(&issue_key) {
        (
            CandidateReference::NativeWork {
                issue: work.issue.clone(),
                change: work.change.clone(),
            },
            IntakeDecision::ExistingWork {
                change: work.change.clone(),
            },
        )
    } else if let Some(proposal) = &issue.related_proposal {
        (
            CandidateReference::Proposal {
                identity: proposal.clone(),
            },
            IntakeDecision::ExistingProposal {
                identity: proposal.clone(),
            },
        )
    } else {
        (
            CandidateReference::Issue {
                identity: issue.identity.clone(),
                state: issue.state,
            },
            match issue.state {
                IssueState::Open => IntakeDecision::ReuseIssue {
                    identity: issue.identity.clone(),
                },
                IssueState::Closed => IntakeDecision::ClosedIssue {
                    identity: issue.identity.clone(),
                },
            },
        )
    };
    matches.entry(reference_key(&pair.0)).or_insert(pair);
}

fn exact_matches(
    digest: &str,
    issues: &BTreeMap<String, &IssueCandidate>,
    proposals: &BTreeMap<String, &ProposalCandidate>,
    work: &BTreeMap<String, &NativeWorkCandidate>,
) -> Candidates {
    let mut matches = Candidates::new();
    for issue in issues.values().filter(|candidate| {
        candidate.basis == MatchBasis::ExactRequest && candidate.request_digest == digest
    }) {
        insert_issue(&mut matches, issue, work);
    }
    for proposal in proposals.values().filter(|candidate| {
        candidate.basis == MatchBasis::ExactRequest && candidate.request_digest == digest
    }) {
        // A matching issue already carrying native work represents this proposal.
        // Retain standalone proposals linked only by unrelated/nonmatching issues.
        let represented = issues.values().any(|issue| {
            issue.basis == MatchBasis::ExactRequest
                && issue.request_digest == digest
                && issue
                    .related_proposal
                    .as_ref()
                    .is_some_and(|linked| linked.stable_key() == proposal.identity.stable_key())
                && work.contains_key(&issue.identity.stable_key())
        });
        if represented {
            continue;
        }
        let pair = (
            CandidateReference::Proposal {
                identity: proposal.identity.clone(),
            },
            IntakeDecision::ExistingProposal {
                identity: proposal.identity.clone(),
            },
        );
        matches.entry(reference_key(&pair.0)).or_insert(pair);
    }
    // Remove proposal terminals inserted through other matching issue chains
    // after all chains have been evaluated. Native work represents that proposal.
    for issue in issues.values().filter(|candidate| {
        candidate.basis == MatchBasis::ExactRequest && candidate.request_digest == digest
    }) {
        if work.contains_key(&issue.identity.stable_key())
            && let Some(proposal) = &issue.related_proposal
        {
            matches.remove(&proposal.stable_key());
        }
    }
    for candidate in work
        .values()
        .filter(|candidate| candidate.request_digest == digest)
    {
        let pair = (
            CandidateReference::NativeWork {
                issue: candidate.issue.clone(),
                change: candidate.change.clone(),
            },
            IntakeDecision::ExistingWork {
                change: candidate.change.clone(),
            },
        );
        matches.entry(reference_key(&pair.0)).or_insert(pair);
    }
    matches
}

fn similarity_matches(
    issues: &BTreeMap<String, &IssueCandidate>,
    proposals: &BTreeMap<String, &ProposalCandidate>,
    work: &BTreeMap<String, &NativeWorkCandidate>,
) -> Candidates {
    let mut matches = Candidates::new();
    for issue in issues
        .values()
        .filter(|candidate| candidate.basis == MatchBasis::Similarity)
    {
        insert_issue(&mut matches, issue, work);
    }
    for proposal in proposals
        .values()
        .filter(|candidate| candidate.basis == MatchBasis::Similarity)
    {
        let pair = (
            CandidateReference::Proposal {
                identity: proposal.identity.clone(),
            },
            IntakeDecision::ExistingProposal {
                identity: proposal.identity.clone(),
            },
        );
        matches.entry(reference_key(&pair.0)).or_insert(pair);
    }
    matches
}

fn validate_nested_identities(observation: &IntakeObservation) -> Result<()> {
    let mut issues = BTreeMap::new();
    let mut proposals = BTreeMap::new();
    for candidate in observation
        .open_issues
        .candidates()
        .iter()
        .chain(observation.closed_issues.candidates())
    {
        check_identity(
            &mut issues,
            candidate.identity.stable_key(),
            &candidate.identity,
        )?;
        if let Some(proposal) = &candidate.related_proposal {
            check_identity(&mut proposals, proposal.stable_key(), proposal)?;
        }
    }
    for candidate in observation.native_work.candidates() {
        check_identity(&mut issues, candidate.issue.stable_key(), &candidate.issue)?;
    }
    for candidate in observation.proposals.candidates() {
        check_identity(
            &mut proposals,
            candidate.identity.stable_key(),
            &candidate.identity,
        )?;
    }
    Ok(())
}

fn check_identity<'a, T: PartialEq>(
    found: &mut BTreeMap<String, &'a T>,
    key: String,
    value: &'a T,
) -> Result<()> {
    if let Some(previous) = found.insert(key, value)
        && previous != value
    {
        return Err(Error::Conflict(
            "same issue or proposal identity has contradictory metadata".into(),
        ));
    }
    Ok(())
}
