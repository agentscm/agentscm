//! Owner-reviewed operating rules over provider-confirmed events and existing Work.
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agentscm::continuation::{NativeClient, NativeRequest, invoke_native, validate_native_request};
use agentscm::hub::pipeline_rules::{self, PipelineRule, RuleJournal};
use agentscm::hub::work::{
    CheckpointKind, SourceRef, WorkCheckpoint, WorkCheckpointResult, WorkItem, WorkRepositoryScope,
    WorkRole, WorkStart, WorkState,
};
use agentscm::operating_policy::{
    Escalation, MAX_RECOVERY_RECONCILIATIONS, OperatingContract, OperatingRecovery, OperatingWork,
    RecoveryOutcome, RecoveryReconciliation, RequiredEvidence, Scenario, SupportedAction,
    draft_prompt, draft_request_digest, rule_digest, validate_draft,
};
use agentscm::{Error, Result};
use clap::Subcommand;
use serde_json::{Value, json};

use crate::managed_work::{CheckpointAttempt, ManagedWork};
use crate::pipeline_cli::{read_json, root, runner_lease};

#[derive(Debug, Subcommand)]
pub enum OperatingPolicyCommand {
    /// Inspect the installed scope, trigger, recipe, profile, evidence and actor role before review.
    Inspect { rule: String },
    /// Ask an installed native client to draft a constrained contract; does not activate it.
    Draft {
        rule: String,
        #[arg(long)]
        intent: PathBuf,
        #[arg(long, value_enum)]
        client: NativeClient,
        #[arg(long)]
        executable: PathBuf,
        #[arg(long)]
        request_id: String,
        #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=900))]
        timeout_seconds: u64,
    },
    /// Retain exact inspected contract bytes in observation mode. Amendments need current generation.
    Review {
        input: PathBuf,
        #[arg(long)]
        expected_generation: Option<i64>,
    },
    /// Replay independently written cases without creating Work or running actions.
    Rehearse { rule: String, scenarios: PathBuf },
    /// Activate the exact reviewed contract and passing rehearsal after a real observation.
    Activate {
        rule: String,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        rehearsal: String,
        #[arg(long)]
        expected_generation: i64,
    },
    /// Pause future managed effects; preserve source, reservations and history.
    Pause {
        rule: String,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        expected_generation: i64,
    },
    /// Revoke future managed effects. A new reviewed version is required to reactivate.
    Revoke {
        rule: String,
        #[arg(long)]
        digest: String,
        #[arg(long)]
        expected_generation: i64,
    },
    /// Verify one actual merged proposal and evaluate the selected operating rule.
    Observe {
        rule: String,
        #[arg(long)]
        connection: PathBuf,
        #[arg(long)]
        proposal: i64,
        /// Execute newly accepted active-policy dispatches in this foreground invocation.
        #[arg(long)]
        execute: bool,
    },
    /// Acquire durable Work and execute one reserved productive workflow with live authority checks.
    Run { dispatch: String },
    /// Explicitly recover the retained workflow; uncertain action attempts are never replayed.
    Recover {
        dispatch: String,
        #[arg(long)]
        expected_generation: i64,
        /// Reconcile this exact retained administrative request while preserving its unknown outcome.
        #[arg(long, requires = "observed_seq")]
        reconcile_operation: Option<String>,
        /// Current Hub Work sequence explicitly inspected by the owner.
        #[arg(long, requires = "reconcile_operation", value_parser = clap::value_parser!(i64).range(1..))]
        observed_seq: Option<i64>,
    },
    /// Inspect the current contract, retained decisions and dispatch outcomes.
    Status { rule: String },
}

fn installed_rule(journal: &RuleJournal, id: &str) -> Result<PipelineRule> {
    journal
        .rules()?
        .into_iter()
        .find(|r| r.rule_id == id)
        .ok_or_else(|| Error::NotFound("installed pipeline rule".into()))
}

async fn connect(rule: &PipelineRule) -> Result<ManagedWork> {
    ManagedWork::connect(
        &rule.git_repository,
        WorkRepositoryScope {
            provider: rule.provider.clone(),
            host: rule.host.clone(),
            repository: rule.repository.clone(),
            repository_id: Some(rule.provider_repository_id),
        },
    )
    .await
}

/// A receipt proves the historical operation. A replay may return newer Work,
/// but only at the boundary explicitly observed before sending this request.
/// The journal binding separately fences generation, lead identity and scope.
fn require_current_checkpoint(
    observed: &WorkItem,
    expected_seq: Option<i64>,
    result: &WorkCheckpointResult,
    state: WorkState,
) -> Result<()> {
    let current = &result.item;
    let retained_replay = result.receipt.replayed && current.updated_seq == observed.updated_seq;
    if !checkpoint_sequence_valid(expected_seq, result.receipt.event_seq, current.updated_seq)
        || (current.updated_seq != result.receipt.event_seq && !retained_replay)
        || current.state != state
    {
        return Err(Error::Conflict(
            "Work changed beyond the observed checkpoint boundary; inspect current responsibility before recovery".into(),
        ));
    }
    Ok(())
}

fn checkpoint_sequence_valid(expected: Option<i64>, receipt: i64, current: i64) -> bool {
    expected.is_some_and(|sequence| sequence > 0 && sequence < receipt && receipt <= current)
}

/// Only a wholly absent runtime directory or a validated missing run proves
/// no workflow. An existing incomplete journal remains an error, not absence.
fn retained_workflow(data: &Path, run: &str) -> Result<Option<agentscm::skill_workflow::RunView>> {
    match std::fs::symlink_metadata(data.join("skill-workflows")) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }
    match agentscm::skill_workflow::runtime::status(data, run) {
        Ok(view) => Ok(Some(view)),
        Err(Error::NotFound(_)) => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_intent(path: &Path) -> Result<String> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_file() || meta.file_type().is_symlink() {
        return Err(Error::Invalid("intent must be a regular file".into()));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.is_empty() || bytes.len() > 16 * 1024 {
        return Err(Error::Limit("intent must contain 1..16384 bytes".into()));
    }
    String::from_utf8(bytes).map_err(|_| Error::Invalid("intent must be UTF-8".into()))
}

async fn mode(
    journal: &mut RuleJournal,
    id: &str,
    digest: &str,
    generation: i64,
    mode: &str,
    rehearsal: Option<&str>,
) -> Result<Value> {
    if matches!(mode, "paused" | "revoked") {
        return Ok(
            json!({"policy":journal.narrow_operating_mode(id,digest,generation,mode)?,"authority":"trusted_local_owner_narrowing"}),
        );
    }
    let rule = installed_rule(journal, id)?;
    let work = connect(&rule).await?;
    work.authenticate().await?;
    Ok(serde_json::to_value(journal.set_operating_mode(
        id,
        digest,
        generation,
        mode,
        rehearsal,
        &work.identity.actor(),
    )?)?)
}

pub async fn run(data: &Path, command: OperatingPolicyCommand) -> Result<Value> {
    let directory = root(data)?;
    let journal_path = directory.join("state.sqlite");
    let mut journal = RuleJournal::open(&journal_path)?;
    match command {
        OperatingPolicyCommand::Inspect { rule } => journal.inspect_operating_rule(&rule),
        OperatingPolicyCommand::Draft {
            rule: id,
            intent,
            client,
            executable,
            request_id,
            timeout_seconds,
        } => {
            let original = read_intent(&intent)?;
            let identity =
                draft_request_digest(&id, &original, client, &executable, timeout_seconds)?;
            if let Some(retained) = journal.operating_draft(&request_id, &identity)? {
                return Ok(retained);
            }
            let rule = installed_rule(&journal, &id)?;
            let work = connect(&rule).await?;
            let profile = pipeline_rules::read_profile(&rule)?;
            let proposed = OperatingContract {
                schema_version: 1,
                rule_id: id,
                original_intent: original.clone(),
                rule_digest: rule_digest(&rule)?,
                eligible_actors: [work.identity.actor()].into(),
                action: SupportedAction::ProductiveArtifacts,
                required_evidence: RequiredEvidence::CurrentProfileAndWorkflowDependencies,
                escalation: Escalation::RetainForOwner,
                max_dispatches: 1,
                max_action_runtime_ms: profile.timeout_ms,
                max_artifact_bytes: profile.max_output_bytes,
                unresolved: vec![],
            };
            let prompt = draft_prompt(&proposed, &rule)?;
            work.authenticate().await?;
            let request = NativeRequest {
                client,
                executable,
                root: rule.git_repository,
                prompt,
                timeout_seconds,
            };
            validate_native_request(&request)?;
            if let Some(retained) = journal.reserve_operating_draft(&request_id, &identity)? {
                return Ok(retained);
            }
            let response = tokio::task::spawn_blocking(move || invoke_native(&request))
                .await
                .map_err(|_| {
                    Error::Conflict("draft worker interrupted; invocation remains uncertain".into())
                })??;
            let contract: OperatingContract = serde_json::from_str(&response.text)?;
            validate_draft(
                &contract,
                &proposed,
                &installed_rule(&journal, &contract.rule_id)?,
            )?;
            let result = json!({"contract":contract,"native_session_id":response.session_id,"request_id":request_id,"state":"draft_not_active","resolved_rule":journal.inspect_operating_rule(&contract.rule_id)?});
            journal.finish_operating_draft(&request_id, &identity, &result)?;
            Ok(result)
        }
        OperatingPolicyCommand::Review {
            input,
            expected_generation,
        } => {
            let contract: OperatingContract = read_json(&input)?;
            let rule = installed_rule(&journal, &contract.rule_id)?;
            let work = connect(&rule).await?;
            work.authenticate().await?;
            journal.review_operating_policy(
                &contract,
                &work.identity.actor(),
                expected_generation,
            )?;
            journal.inspect_operating_rule(&contract.rule_id)
        }
        OperatingPolicyCommand::Rehearse { rule, scenarios } => {
            let cases: Vec<Scenario> = read_json(&scenarios)?;
            let (digest, report) = journal.rehearse_operating_policy(&rule, cases)?;
            Ok(json!({"digest":digest,"report":report,"operational_effects":false}))
        }
        OperatingPolicyCommand::Activate {
            rule,
            digest,
            rehearsal,
            expected_generation,
        } => {
            mode(
                &mut journal,
                &rule,
                &digest,
                expected_generation,
                "active",
                Some(&rehearsal),
            )
            .await
        }
        OperatingPolicyCommand::Pause {
            rule,
            digest,
            expected_generation,
        } => {
            mode(
                &mut journal,
                &rule,
                &digest,
                expected_generation,
                "paused",
                None,
            )
            .await
        }
        OperatingPolicyCommand::Revoke {
            rule,
            digest,
            expected_generation,
        } => {
            mode(
                &mut journal,
                &rule,
                &digest,
                expected_generation,
                "revoked",
                None,
            )
            .await
        }
        OperatingPolicyCommand::Status { rule } => {
            let history = journal.operating_history(&rule)?;
            let dispatches: Vec<_> = journal
                .dispatches(256)?
                .into_iter()
                .filter(|d| d.dispatch.rule.rule_id == rule)
                .collect();
            let work_bindings = dispatches.iter().map(|d| {
                Ok(json!({"dispatch_id":d.dispatch.dispatch_id,"binding":journal.operating_work(&d.dispatch.dispatch_id)?}))
            }).collect::<Result<Vec<_>>>()?;
            Ok(
                json!({"history":history,"dispatches":dispatches,"work_bindings":work_bindings,"resolved_rule":journal.inspect_operating_rule(&rule)?}),
            )
        }
        OperatingPolicyCommand::Observe {
            rule: id,
            connection,
            proposal,
            execute,
        } => {
            let installed = installed_rule(&journal, &id)?;
            let work = connect(&installed).await?;
            let event = crate::pipeline_cli::confirm(&connection, proposal).await?;
            work.authenticate().await?;
            if event.provider != installed.provider
                || event.host != installed.host
                || event.repository != installed.repository
                || event.provider_repository_id != installed.provider_repository_id
            {
                return Err(Error::Conflict(
                    "confirmed event belongs to another installed repository".into(),
                ));
            }
            let accepted = journal.accept_operating_event(&id, &event, &work.identity.actor())?;
            let policy = journal.operating_policy(&id)?;
            drop(journal);
            let mut executions = Vec::new();
            if execute {
                for dispatch in &accepted {
                    executions.push(
                        Box::pin(run(
                            data,
                            OperatingPolicyCommand::Run {
                                dispatch: dispatch.dispatch_id.clone(),
                            },
                        ))
                        .await?,
                    );
                }
            }
            Ok(
                json!({"event":event,"accepted":accepted,"policy":policy,"executions":executions,"execution_requested":execute}),
            )
        }
        OperatingPolicyCommand::Run { dispatch } => {
            let _runner = runner_lease(&directory)?;
            let retained = journal.dispatch(&dispatch)?;
            let installed = &retained.dispatch.rule;
            let policy = journal.operating_policy(&installed.rule_id)?;
            let managed = Arc::new(connect(installed).await?);
            if !policy
                .contract
                .eligible_actors
                .contains(&managed.identity.actor())
            {
                return Err(Error::Conflict(
                    "current actor is not allowed by operating policy".into(),
                ));
            }
            if retained.state == "complete" {
                return Ok(json!({"dispatch":retained,"replayed":true}));
            }
            if retained.state != "pending" {
                return Err(Error::Conflict(
                    "dispatch requires explicit recovery; inspect retained workflow uncertainty"
                        .into(),
                ));
            }
            journal.check_operating_dispatch(&dispatch, &managed.identity.actor())?;
            let previous = journal.operating_work(&dispatch)?;
            let work = if let Some(binding) = &previous {
                if binding.identity_commitment != managed.identity_commitment()? {
                    return Err(Error::Conflict(
                        "Hub identity changed since Work reservation".into(),
                    ));
                }
                managed.check_current(&binding.work).await?
            } else {
                managed
                    .start(&WorkStart {
                        work_id: None,
                        source: None,
                        request_key: Some(retained.dispatch.run_id.clone()),
                        title: format!(
                            "{} after merge #{}",
                            installed.rule_id, retained.dispatch.event.proposal_number
                        ),
                        session: format!("operating-{}", retained.dispatch.dispatch_id),
                        role: WorkRole::Lead,
                        scope: vec![],
                        branch: None,
                        worktree: None,
                        next_action: Some("Run the reviewed local artifact workflow".into()),
                        depends_on: vec![],
                        related_work: vec![],
                        takeover_generation: None,
                        preflight: None,
                    })
                    .await?
            };
            let binding = OperatingWork {
                identity_commitment: managed.identity_commitment()?,
                work: work.clone(),
                recovery: previous.as_ref().and_then(|b| b.recovery.clone()),
                recovery_reconciliations: previous
                    .as_ref()
                    .map(|b| b.recovery_reconciliations.clone())
                    .unwrap_or_default(),
            };
            journal.bind_operating_work(&dispatch, previous.as_ref(), &binding)?;
            managed.check_current(&work).await?;
            let claim = journal.claim(&dispatch)?;
            let work_dir = directory.join("work");
            if !work_dir.exists() {
                use std::os::unix::fs::DirBuilderExt;
                std::fs::DirBuilder::new().mode(0o700).create(&work_dir)?;
            }
            let handle = tokio::runtime::Handle::current();
            let current = managed.clone();
            let expected = work.clone();
            let data = data.to_path_buf();
            let execution = tokio::task::spawn_blocking(move || {
                let mut journal = RuleJournal::open(&journal_path)?;
                let guard = || {
                    handle
                        .block_on(current.check_current(&expected))
                        .map(|_| ())
                };
                pipeline_rules::deliver_with_operating_guard(
                    &data,
                    &mut journal,
                    &claim,
                    &work_dir,
                    &agentscm::execution::Cancellation::default(),
                    &guard,
                )
            })
            .await
            .map_err(|_| {
                Error::Conflict("managed run interrupted; inspect before recovery".into())
            })?;
            let succeeded = execution
                .as_ref()
                .is_ok_and(|v| v.state == agentscm::skill_workflow::RunState::Succeeded);
            let result = match execution {
                Ok(view) => json!({"run":view}),
                Err(error) => json!({"blocked":error.to_string()}),
            };
            let checkpoint = managed
                .checkpoint(&WorkCheckpoint {
                    work_id: work.id.clone(),
                    operation_id: format!(
                        "operating-result-{}-{}",
                        dispatch,
                        retained.claim_generation + 1
                    ),
                    generation: work.generation,
                    session: work.lead_session.clone(),
                    expected_updated_seq: Some(work.updated_seq),
                    kind: if succeeded {
                        CheckpointKind::Finish
                    } else {
                        CheckpointKind::Blocker
                    },
                    summary: if succeeded {
                        "Reviewed operating workflow produced retained local artifacts"
                    } else {
                        "Operating workflow blocked; inspect its retained run before recovery"
                    }
                    .into(),
                    next_action: Some(
                        "Inspect the pipeline dispatch and workflow artifacts".into(),
                    ),
                    scope: None,
                    branch: None,
                    worktree: None,
                    resolve_blocker: false,
                    source_refs: vec![SourceRef::Commit {
                        oid: retained.dispatch.event.accepted_commit,
                        path: None,
                    }],
                    depends_on: vec![],
                    related_work: vec![],
                })
                .await
                .and_then(|recorded| {
                    require_current_checkpoint(
                        &work,
                        Some(work.updated_seq),
                        &recorded,
                        if succeeded {
                            WorkState::Finished
                        } else {
                            WorkState::Blocked
                        },
                    )?;
                    Ok(recorded)
                });
            if let Ok(recorded) = &checkpoint {
                journal.bind_operating_work(
                    &dispatch,
                    Some(&binding),
                    &OperatingWork {
                        identity_commitment: binding.identity_commitment.clone(),
                        work: recorded.item.clone(),
                        recovery: binding.recovery.clone(),
                        recovery_reconciliations: binding.recovery_reconciliations.clone(),
                    },
                )?;
            }
            Ok(
                json!({"work_id":work.id,"execution":result,"checkpoint":checkpoint.as_ref().ok(),"checkpoint_recorded":checkpoint.is_ok(),"checkpoint_error":checkpoint.err().map(|_|"Work checkpoint not confirmed; inspect current Work before retry")}),
            )
        }
        OperatingPolicyCommand::Recover {
            dispatch,
            expected_generation,
            reconcile_operation,
            observed_seq,
        } => {
            let _runner = runner_lease(&directory)?;
            let retained = journal.dispatch(&dispatch)?;
            let policy = journal.operating_policy(&retained.dispatch.rule.rule_id)?;
            let managed = connect(&retained.dispatch.rule).await?;
            if policy.reviewer != managed.identity.actor() || policy.mode != "active" {
                return Err(Error::Conflict(
                    "only the current owner of an active policy can recover work".into(),
                ));
            }
            if reconcile_operation.is_none()
                && retained.state == "pending"
                && retained.claim_generation == expected_generation.saturating_add(1)
                && journal.operating_work(&dispatch)?.is_some_and(|b| {
                    b.recovery.as_ref().is_some_and(|r| {
                        r.dispatch_generation == expected_generation && r.rejected_at_seq.is_none()
                    })
                })
            {
                managed.authenticate().await?;
                return Ok(
                    json!({"dispatch":retained,"replayed":true,"next":"Run the retained pending dispatch"}),
                );
            }
            if retained.claim_generation != expected_generation {
                return Err(Error::Conflict(
                    "dispatch recovery generation conflict".into(),
                ));
            }
            if !matches!(retained.state.as_str(), "claimed" | "blocked" | "complete") {
                return Err(Error::Conflict(
                    "dispatch is already pending; use run".into(),
                ));
            }
            journal.check_operating_dispatch(&dispatch, &managed.identity.actor())?;
            let Some(binding) = journal.operating_work(&dispatch)? else {
                if reconcile_operation.is_some() {
                    return Err(Error::Conflict(
                        "no retained administrative request to reconcile".into(),
                    ));
                }
                // Older legacy pickup may have claimed then refused this dispatch before Work.
                // Requeue only when no productive workflow exists; never adopt uncertain effects.
                if retained.state == "complete" {
                    return Err(Error::Conflict(
                        "completed dispatch has no retained Work binding".into(),
                    ));
                }
                if retained_workflow(data, &retained.dispatch.run_id)?.is_some() {
                    return Err(Error::Conflict("bindingless dispatch has retained workflow state; inspect its effects before recovery".into()));
                }
                managed.authenticate().await?;
                journal.recover(&dispatch, expected_generation)?;
                return Ok(
                    json!({"dispatch":journal.dispatch(&dispatch)?,"next":"Run the pending dispatch to acquire its first Work binding"}),
                );
            };
            managed.authenticate().await?;
            let current = managed.read(&binding.work.id).await?;
            let completed = retained.state == "complete";
            let mut replay = binding.recovery.as_ref().filter(|r| {
                r.dispatch_generation == expected_generation && r.rejected_at_seq.is_none()
            });
            let expected_kind = if completed {
                CheckpointKind::Finish
            } else {
                CheckpointKind::Progress
            };
            if binding.identity_commitment != managed.identity_commitment()?
                || current.generation != binding.work.generation
                || current.lead_token != binding.work.lead_token
                || current.lead_session != binding.work.lead_session
                || current.scope != binding.work.scope
                || replay.is_some_and(|r| r.request.kind != expected_kind)
                || !(matches!(current.state, WorkState::Active | WorkState::Blocked)
                    || completed && current.state == WorkState::Finished)
            {
                return Err(Error::Conflict(
                    "Work authority changed; inspect before explicit recovery".into(),
                ));
            }
            let mut recovery_reconciliations = binding.recovery_reconciliations.clone();
            if let Some(operation) = &reconcile_operation {
                let previous = replay.ok_or_else(|| Error::Conflict(
                    "reconciliation requires the exact retained request with an unresolved outcome".into()))?;
                if previous.request.operation_id != *operation
                    || observed_seq != Some(current.updated_seq)
                    || previous
                        .request
                        .expected_updated_seq
                        .is_none_or(|s| s >= current.updated_seq)
                {
                    return Err(Error::Conflict("retained operation or observed Work sequence differs; inspect before reconciliation".into()));
                }
                let workflow = retained_workflow(data, &retained.dispatch.run_id)?;
                use agentscm::skill_workflow::RunState;
                let safe_state = if completed {
                    workflow.is_some_and(|view| view.state == RunState::Succeeded)
                } else {
                    workflow.is_none_or(|view| {
                        matches!(
                            view.state,
                            RunState::Failed | RunState::Blocked | RunState::Cancelled
                        )
                    })
                };
                if !safe_state {
                    return Err(Error::Conflict("retained workflow is not in a terminal state appropriate for reconciliation".into()));
                }
                if recovery_reconciliations.len() >= MAX_RECOVERY_RECONCILIATIONS {
                    return Err(Error::Limit(
                        "recovery reconciliation history is full; retained evidence preserved"
                            .into(),
                    ));
                }
                recovery_reconciliations.push(RecoveryReconciliation {
                    recovery: previous.clone(),
                    observed_seq: current.updated_seq,
                    outcome: RecoveryOutcome::Unknown,
                });
                replay = None;
            }
            if reconcile_operation.is_none()
                && completed
                && current.state == WorkState::Finished
                && replay.is_none()
            {
                let matching = current.current.as_ref().is_some_and(|c| c.kind == CheckpointKind::Finish
                    && c.source_refs.iter().any(|s| matches!(s, SourceRef::Commit {oid,path:None} if oid == &retained.dispatch.event.accepted_commit)));
                if !matching {
                    return Err(Error::Conflict("finished Work lacks the matching result checkpoint; inspect before reconciliation".into()));
                }
                journal.bind_operating_work(
                    &dispatch,
                    Some(&binding),
                    &OperatingWork {
                        identity_commitment: binding.identity_commitment.clone(),
                        work: current,
                        recovery: binding.recovery.clone(),
                        recovery_reconciliations: binding.recovery_reconciliations.clone(),
                    },
                )?;
                return Ok(
                    json!({"dispatch":retained,"work":journal.operating_work(&dispatch)?,"reconciled":true}),
                );
            }
            let request = replay.map(|r| r.request.clone()).unwrap_or_else(|| WorkCheckpoint {
                work_id: current.id.clone(), operation_id: format!("operating-recovery-{dispatch}-{expected_generation}-s{}",current.updated_seq),
                generation: current.generation, session: current.lead_session.clone(), expected_updated_seq: Some(current.updated_seq),
                kind: if completed { CheckpointKind::Finish } else { CheckpointKind::Progress },
                summary: if completed { "Reconciled retained successful workflow" } else { "Explicitly resumed retained workflow; uncertain actions remain non-replayable" }.into(),
                next_action: Some("Inspect retained workflow artifacts and attempts".into()), scope: None, branch: None, worktree: None,
                resolve_blocker: true,
                source_refs: if completed { vec![SourceRef::Commit {
                    oid: retained.dispatch.event.accepted_commit.clone(), path: None,
                }] } else { vec![] },
                depends_on: vec![], related_work: vec![],
            });
            let reserved = OperatingWork {
                identity_commitment: binding.identity_commitment.clone(),
                work: current,
                recovery_reconciliations,
                recovery: Some(OperatingRecovery {
                    dispatch_generation: expected_generation,
                    request: request.clone(),
                    rejected_at_seq: None,
                }),
            };
            journal.bind_operating_work(&dispatch, Some(&binding), &reserved)?;
            let checkpoint = match managed.checkpoint_for_recovery(&request).await? {
                CheckpointAttempt::Recorded(checkpoint) => *checkpoint,
                CheckpointAttempt::RejectedSequence(sequence) => {
                    let mut rejected = reserved.clone();
                    rejected
                        .recovery
                        .as_mut()
                        .expect("recovery reserved above")
                        .rejected_at_seq = Some(sequence);
                    journal.bind_operating_work(&dispatch, Some(&reserved), &rejected)?;
                    return Err(Error::Conflict("Hub rejected the recovery checkpoint without committing it because Work changed; inspect Work and retry recovery explicitly".into()));
                }
            };
            require_current_checkpoint(
                &reserved.work,
                request.expected_updated_seq,
                &checkpoint,
                if completed {
                    WorkState::Finished
                } else {
                    WorkState::Active
                },
            )?;
            journal.bind_operating_work(
                &dispatch,
                Some(&reserved),
                &OperatingWork {
                    identity_commitment: reserved.identity_commitment.clone(),
                    work: checkpoint.item,
                    recovery: reserved.recovery.clone(),
                    recovery_reconciliations: reserved.recovery_reconciliations.clone(),
                },
            )?;
            if !completed {
                journal.recover(&dispatch, expected_generation)?;
            }
            Ok(
                json!({"dispatch":journal.dispatch(&dispatch)?,"work":journal.operating_work(&dispatch)?,"next":"Run the retained dispatch; existing uncertain action attempts will not be replayed"}),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::checkpoint_sequence_valid;

    #[test]
    fn recovery_receipts_must_follow_the_request_and_not_exceed_current_work() {
        assert!(checkpoint_sequence_valid(Some(10), 11, 12));
        assert!(checkpoint_sequence_valid(Some(10), 12, 12));
        assert!(!checkpoint_sequence_valid(Some(10), 10, 12));
        assert!(!checkpoint_sequence_valid(Some(10), 9, 12));
        assert!(!checkpoint_sequence_valid(Some(10), 13, 12));
        assert!(!checkpoint_sequence_valid(None, 11, 12));
    }
}
